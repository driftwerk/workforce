//! Metabase agent-callable tools.
//!
//! Every tool accepts an optional `instance` parameter for multi-instance
//! selection (explicit → default). Card/dashboard descriptions and query
//! result rows are wrapped via `wrap_external_content` to defend against
//! prompt injection.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use tracing::{info, warn};

use workforce_agent::artifacts::tabular::wrap_envelope_preview;
use workforce_agent::artifacts::{
    global_store, normalize_columns, stage_table, zip_table, StagedTable, DEFAULT_INLINE_THRESHOLD,
};
use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::client::{field_names, qualified_table_name, MetabaseClient};
use super::config::clamp_limit;
use super::schema_prompt::DATABASE_ID_STATIC_DESCRIPTION;
use crate::adapters::utils::{coerce_to_i64, coerce_to_u64, instance_choices};
use workforce_common::metabase_schema::names_same_table;

// ── shared helpers ──────────────────────────────────────────────────

fn resolve_client(
    clients: &HashMap<String, Arc<MetabaseClient>>,
    default_instance: &str,
    input: &Value,
) -> eyre::Result<Arc<MetabaseClient>> {
    let name = workforce_agent::llm::environment::resolve_instance(
        "metabase",
        input["instance"].as_str(),
        default_instance,
        clients,
    )
    .map_err(|d| eyre::eyre!("{}", d.message))?;
    clients.get(&name).cloned().ok_or_else(|| {
        eyre::eyre!(
            "Unknown Metabase instance: '{name}'. Available: {:?}",
            clients.keys().collect::<Vec<_>>()
        )
    })
}

fn instance_description(
    clients: &HashMap<String, Arc<MetabaseClient>>,
    default_instance: &str,
) -> String {
    if clients.len() <= 1 {
        return "Metabase instance name (for multi-instance setups)".to_string();
    }
    format!(
        "Metabase instance. {}",
        instance_choices(clients, default_instance, "instance")
    )
}

fn wrap_description(mut value: Value, source: &str) -> Value {
    if let Some(obj) = value.as_object_mut() {
        if let Some(desc) = obj.get("description").and_then(|d| d.as_str()) {
            if !desc.is_empty() {
                let wrapped = wrap_external_content(desc, source).wrapped_content;
                obj.insert("description".to_string(), Value::String(wrapped));
            }
        }
    }
    value
}

fn instance_schema_prop(
    clients: &HashMap<String, Arc<MetabaseClient>>,
    default_instance: &str,
) -> Value {
    json!({
        "type": "string",
        "description": instance_description(clients, default_instance)
    })
}

/// Slim the `data` block of a Metabase dataset response: keep `cols` and
/// `rows_truncated`, drop heavy fields (`native_form`, `insights`), and
/// replace the raw `rows` array with either a wrapped inline string
/// (`rows_wrapped`) or a staged `tabular_artifact` envelope. Injection
/// markers cover every cell that crosses the trust boundary.
fn slim_dataset_data(data: &Value, source: &'static str) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(v) = data.get("cols") {
        out.insert("cols".to_string(), v.clone());
    }
    if let Some(v) = data.get("rows_truncated") {
        out.insert("rows_truncated".to_string(), v.clone());
    }
    if let Some(rows) = data.get("rows").and_then(|r| r.as_array()) {
        match offload_rows(rows, column_names(data.get("cols")), source) {
            OffloadedRows::Inline(rendered) => {
                out.insert("row_count".to_string(), Value::from(rows.len()));
                let wrapped = wrap_external_content(&rendered, source).wrapped_content;
                out.insert("rows_wrapped".to_string(), Value::String(wrapped));
            }
            OffloadedRows::Staged(envelope) => {
                if let Value::Object(fields) = envelope {
                    out.extend(fields);
                }
            }
        }
    }
    Value::Object(out)
}

enum OffloadedRows {
    Inline(String),
    Staged(Value),
}

/// Inline when the rendered payload fits the threshold; otherwise zip the
/// Metabase array rows into canonical row objects and stage them, wrapping
/// the preview (external content) before it joins the envelope.
fn offload_rows(
    rows: &[Value],
    schema: Option<Vec<String>>,
    source: &'static str,
) -> OffloadedRows {
    let rendered = serde_json::to_string(rows).unwrap_or_default();
    if rendered.len() <= DEFAULT_INLINE_THRESHOLD {
        return OffloadedRows::Inline(rendered);
    }
    let Some(store) = global_store() else {
        return OffloadedRows::Inline(rendered);
    };
    let Some(agent_id) = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty())
    else {
        return OffloadedRows::Inline(rendered);
    };
    let width = rows
        .iter()
        .map(|r| r.as_array().map_or(1, Vec::len))
        .max()
        .unwrap_or(0);
    let mut raw_columns = schema.unwrap_or_default();
    if raw_columns.len() < width {
        raw_columns.resize(width, String::new());
    }
    let columns = normalize_columns(raw_columns);
    let cells = rows
        .iter()
        .map(|r| r.as_array().cloned().unwrap_or_else(|| vec![r.clone()]))
        .collect();
    let objects = zip_table(&columns, cells);
    match stage_table(
        &*store,
        &agent_id,
        source,
        Some(columns),
        objects,
        DEFAULT_INLINE_THRESHOLD,
    ) {
        Ok(StagedTable::Staged { mut envelope, .. }) => {
            wrap_envelope_preview(&mut envelope, source);
            OffloadedRows::Staged(envelope)
        }
        Ok(StagedTable::Inline { .. }) => OffloadedRows::Inline(rendered),
        Err(e) => {
            warn!(error = %e, "Failed to stage dataset rows as artifact; inlining");
            OffloadedRows::Inline(rendered)
        }
    }
}

fn column_names(cols: Option<&Value>) -> Option<Vec<String>> {
    let arr = cols?.as_array()?;
    Some(
        arr.iter()
            .map(|c| {
                c.get("name")
                    .or_else(|| c.get("display_name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
    )
}

/// Copy every non-null value from `input` whose key is in `keys` into `body`.
/// Used to build partial update payloads without boilerplate.
fn merge_fields(body: &mut Value, input: &Value, keys: &[&str]) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for key in keys {
        if let Some(v) = input.get(*key) {
            if !v.is_null() {
                obj.insert((*key).to_string(), v.clone());
            }
        }
    }
}

#[derive(Clone)]
struct Ctx {
    clients: Arc<HashMap<String, Arc<MetabaseClient>>>,
    default_instance: String,
}

impl Ctx {
    fn client(&self, input: &Value) -> Result<Arc<MetabaseClient>> {
        resolve_client(&self.clients, &self.default_instance, input)
    }
    fn instance_prop(&self) -> Value {
        instance_schema_prop(&self.clients, &self.default_instance)
    }
}

macro_rules! metabase_tool {
    ($struct:ident) => {
        pub struct $struct {
            ctx: Ctx,
        }
        impl $struct {
            pub fn new(
                clients: Arc<HashMap<String, Arc<MetabaseClient>>>,
                default_instance: String,
            ) -> Self {
                Self {
                    ctx: Ctx {
                        clients,
                        default_instance,
                    },
                }
            }
        }
    };
}

async fn run<F, Fut>(ctx: &Ctx, input: Value, f: F) -> Result<ToolResult>
where
    F: FnOnce(Arc<MetabaseClient>, Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    let start = std::time::Instant::now();
    let client = match ctx.client(&input) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ToolResult::failure(
                e.to_string(),
                start.elapsed().as_millis() as u64,
            ))
        }
    };
    match f(client, input).await {
        Ok(output) => Ok(ToolResult::success(
            output,
            start.elapsed().as_millis() as u64,
        )),
        Err(e) => {
            warn!(error = %e, "Metabase tool failed");
            Ok(ToolResult::from_report(
                &e,
                start.elapsed().as_millis() as u64,
            ))
        }
    }
}

/// Engines verified to reject ILIKE. Allowlist-of-bad: unknown engines are
/// never linted (fail open).
const NO_ILIKE_ENGINES: &[&str] = &[
    "mysql",
    "mariadb",
    "sqlserver",
    "oracle",
    "h2",
    "sqlite",
    "bigquery-cloud-sdk",
];

/// True when the engine is known to reject ILIKE and the SQL uses it as a
/// word (outside string literals, case-insensitive).
fn ilike_rejected(sql: &str, engine: &str) -> bool {
    NO_ILIKE_ENGINES.contains(&engine.to_ascii_lowercase().as_str())
        && strip_sql_noise(sql)
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|tok| tok.eq_ignore_ascii_case("ilike"))
}

/// A database error that amounts to "that name isn't real", and so can be
/// answered by handing back the names that are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// `qualifier` is the table alias or name the column was written against
    /// (`r` in `r.hospital_id`), when it was qualified at all.
    UnknownColumn {
        qualifier: Option<String>,
        column: String,
    },
    UnknownTable {
        name: String,
    },
}

/// Extract the quoted or bare identifier following `marker` in `lower`, taken
/// from `error` so the original casing survives.
///
/// `lower` MUST be an ASCII-lowercased copy of `error`: the byte index comes
/// from one string and slices the other, and full `to_lowercase` can change
/// byte length (`İ` → `i̇`), which orders the slice and mis-reads the name.
fn identifier_after(error: &str, lower: &str, marker: &str) -> Option<String> {
    let rest = &error[lower.find(marker)? + marker.len()..];
    let rest = rest.trim_start();
    let quote = rest.chars().next().filter(|c| "'\"`".contains(*c));
    let ident = match quote {
        Some(q) => rest[q.len_utf8()..].split(q).next()?,
        // Unquoted (Postgres): the identifier runs to the first space.
        None => rest.split_whitespace().next()?,
    };
    let ident = ident.trim();
    (!ident.is_empty()).then(|| ident.to_string())
}

/// Split `a.b` into qualifier + bare name, on the LAST dot so a
/// schema-qualified table (`harbor.organisations`) and an alias-qualified
/// column (`r.hospital_id`) both reduce correctly.
fn split_qualifier(ident: &str) -> (Option<String>, String) {
    match ident.rsplit_once('.') {
        Some((q, name)) if !q.is_empty() && !name.is_empty() => {
            (Some(q.to_string()), name.to_string())
        }
        _ => (None, ident.to_string()),
    }
}

const UNKNOWN_COLUMN_MARKERS: &[&str] = &["unknown column ", "invalid column name ", "column "];
const UNKNOWN_TABLE_MARKERS: &[&str] = &["table ", "invalid object name ", "relation "];

/// Blank string literals and drop comments, so neither can be mistaken for
/// SQL. Both callers need it: `-- from audit_log` would otherwise invent a
/// table for [`table_aliases`], and `-- avoid ILIKE here` would trip the
/// dialect lint on a query that never uses it.
///
/// One pass, tracking both states, because they nest: stripping literals first
/// lets an apostrophe inside a comment (`-- don't include cancelled`) open a
/// literal that blanks the rest of the query, and stripping comments first
/// lets a `--` inside a literal truncate it. Neither order is safe; only
/// handling them together is.
fn strip_sql_noise(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push('\'');
                while let Some(inner) = chars.next() {
                    if inner != '\'' {
                        continue;
                    }
                    match chars.peek() {
                        Some('\'') => {
                            chars.next();
                        }
                        _ => {
                            out.push('\'');
                            break;
                        }
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for skipped in chars.by_ref() {
                    if prev == '*' && skipped == '/' {
                        break;
                    }
                    prev = skipped;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Classify a database error into the question it is really asking.
///
/// Column patterns are tried FIRST because Postgres reports both kinds with
/// `does not exist` — testing tables first would classify
/// `column jp.deleted does not exist` as a missing table and answer with a
/// table list, which is the wrong question.
pub fn classify_schema_error(error: &str) -> Option<SchemaError> {
    let lower = error.to_ascii_lowercase();
    let missing = |m: &str| lower.contains(m);
    let unknown = missing("unknown")
        || missing("invalid")
        || missing("does not exist")
        || missing("doesn't exist")
        // BigQuery: "Not found: Table proj:ds.tbl was not found in location EU".
        || missing("not found");
    if !unknown {
        return None;
    }

    for marker in UNKNOWN_COLUMN_MARKERS {
        if let Some(ident) = identifier_after(error, &lower, marker) {
            let (qualifier, column) = split_qualifier(&ident);
            return Some(SchemaError::UnknownColumn { qualifier, column });
        }
    }
    for marker in UNKNOWN_TABLE_MARKERS {
        if let Some(ident) = identifier_after(error, &lower, marker) {
            // A schema qualifier (`harbor.organisations`) is noise here —
            // the agent needs the bare name to match against the table list.
            return Some(SchemaError::UnknownTable {
                name: split_qualifier(&ident).1,
            });
        }
    }
    None
}

const TABLE_LIST_MAX_NAMES: usize = 60;
const TABLE_LIST_MAX_CHARS: usize = 2_000;

/// Caps on the schema hint appended to a failed query. Sized to contain the
/// answer without distorting the round: the runner keeps the most recent tool
/// result at full fidelity, and the token estimator is known to under-count.
const SCHEMA_HINT_MAX_BYTES: usize = 4_000;
const MAX_DESCRIBED_TABLES: usize = 4;
const MAX_COLUMN_NAMES: usize = 80;

/// Strip one layer of SQL identifier quoting.
fn unquote_identifier(raw: &str) -> &str {
    raw.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']')
}

/// Tables a query reads, as `(table, alias)`, from the tokens following
/// `FROM` / `JOIN`.
///
/// Runs over literal-stripped SQL so a string containing the word `from`
/// cannot invent a table. Deliberately shallow — it only has to be right often
/// enough to name the table the agent meant; when it is wrong the caller falls
/// back to describing everything it did find.
fn table_aliases(sql: &str) -> Vec<(String, Option<String>)> {
    const KEYWORDS: &[&str] = &[
        "on",
        "where",
        "join",
        "inner",
        "left",
        "right",
        "outer",
        "cross",
        "group",
        "order",
        "having",
        "limit",
        "union",
        "set",
        "using",
        "select",
        "and",
        "or",
        "full",
        "natural",
        "lateral",
        "straight_join",
    ];
    let stripped = strip_sql_noise(sql);
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut push = |table: String, alias: Option<String>| {
        if !out.iter().any(|(t, a)| *t == table && *a == alias) {
            out.push((table, alias));
        }
    };

    for (i, tok) in tokens.iter().enumerate() {
        if !tok.eq_ignore_ascii_case("from") && !tok.eq_ignore_ascii_case("join") {
            continue;
        }
        // A comma-separated list is still one FROM clause, so keep consuming
        // while a token ends with a comma — otherwise `FROM users, orders`
        // reads `orders` as an alias and loses it entirely.
        let mut j = i + 1;
        while let Some(raw) = tokens.get(j) {
            let table_trails = raw.ends_with(',');
            let Some(table) = table_name(raw) else { break };
            j += 1;

            let mut alias = None;
            let mut alias_trails = false;
            // `FROM (SELECT … FROM orders) o` — the `)` closes a derived table,
            // so `o` names the subquery, not `orders`. Binding it would offer
            // the base table's columns as the alias's, listing a column and
            // denying it in the same breath.
            let closes_subquery = raw.ends_with(')');
            if !table_trails && !closes_subquery {
                match tokens.get(j) {
                    Some(t) if t.eq_ignore_ascii_case("as") => {
                        alias = tokens.get(j + 1).and_then(|a| {
                            alias_trails = a.ends_with(',');
                            identifier_token(a)
                        });
                        j += 2;
                    }
                    Some(t) if !KEYWORDS.iter().any(|k| t.eq_ignore_ascii_case(k)) => {
                        alias_trails = t.ends_with(',');
                        alias = identifier_token(t);
                        j += 1;
                    }
                    _ => {}
                }
            }
            push(table, alias);
            if !table_trails && !alias_trails {
                break;
            }
        }
    }
    out
}

/// A bare identifier from one SQL token: trailing punctuation removed, then
/// unquoted. `None` for a subquery or anything that isn't a name.
fn identifier_token(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_end_matches([',', ';', ')'])
        .trim_start_matches('(');
    let ident = unquote_identifier(trimmed);
    (!ident.is_empty() && !raw.starts_with('(')).then(|| ident.to_string())
}

/// The bare table name from one SQL token. The qualifier is split off BEFORE
/// unquoting, or `` `db`.`users` `` would keep an inner backtick and match no
/// table.
fn table_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches([',', ';', ')']);
    if trimmed.is_empty() || raw.starts_with('(') {
        return None;
    }
    let name = unquote_identifier(&split_qualifier(trimmed).1).to_string();
    (!name.is_empty()).then_some(name)
}

/// Names bound by a `WITH … AS (…)` clause. They read exactly like tables but
/// exist only inside the query, so counting them as referenced tables would
/// make every CTE query look partially describable.
fn cte_names(sql: &str) -> Vec<String> {
    let stripped = strip_sql_noise(sql);
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    tokens
        .windows(3)
        .filter(|w| w[1].eq_ignore_ascii_case("as") && w[2].starts_with('('))
        .filter_map(|w| identifier_token(w[0]))
        .collect()
}

/// The table a failed column reference points at: the alias's table when the
/// qualifier resolves, else every table the query reads.
/// Every real table the query reaches for, deduplicated, CTEs excluded.
///
/// The ranking signal behind the tool schema's hot set: a table nobody queries
/// does not earn prompt bytes. Deliberately built from the same parse
/// [`tables_for_column`] uses, so the two can never disagree about what a query
/// referenced. Names are not resolved against the real schema here — a name
/// that turns out not to exist is exactly the miss worth counting.
fn referenced_table_names(sql: &str) -> Vec<String> {
    let ctes = cte_names(sql);
    let mut seen: Vec<String> = Vec::new();
    for (table, _) in table_aliases(sql) {
        if ctes.iter().any(|c| c.eq_ignore_ascii_case(&table)) {
            continue;
        }
        if !plausible_name(&table) {
            continue;
        }
        if !seen.iter().any(|t| t.eq_ignore_ascii_case(&table)) {
            seen.push(table);
        }
    }
    seen
}

fn tables_for_column(sql: &str, qualifier: Option<&str>) -> (Vec<String>, bool) {
    let ctes = cte_names(sql);
    let refs: Vec<(String, Option<String>)> = table_aliases(sql)
        .into_iter()
        .filter(|(table, _)| !ctes.iter().any(|c| c.eq_ignore_ascii_case(table)))
        .collect();
    if let Some(q) = qualifier {
        let matched = refs.iter().find(|(table, alias)| {
            alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q))
                || table.eq_ignore_ascii_case(q)
        });
        if let Some((table, _)) = matched {
            return (vec![table.clone()], true);
        }
    }
    (refs.into_iter().map(|(t, _)| t).collect(), false)
}

/// The `tables[]` entries of a `/database/{id}/metadata` response that the
/// query actually references, in query order, capped — resolved against the
/// real schema BEFORE the cap so a mis-parsed name cannot crowd out a real
/// table. Each comes back under the name the snapshot keys it by, so the
/// columns learned here are the ones the tool schema later spells out.
fn referenced_tables<'a>(metadata: &'a Value, wanted: &[String]) -> Vec<(i64, String, &'a Value)> {
    let entries = metadata
        .get("tables")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut found: Vec<(i64, String, &Value)> = Vec::new();
    for name in wanted {
        let matched = entries.iter().find_map(|entry| {
            let real = entry.get("name")?.as_str()?;
            let id = entry.get("id").and_then(coerce_to_i64)?;
            real.eq_ignore_ascii_case(name)
                .then(|| qualified_table_name(entry))
                .flatten()
                .map(|qualified| (id, qualified, entry))
        });
        // A self-join names one table twice; describing it twice wastes a slot
        // and a round-trip and reads as though there were two.
        if let Some(hit) = matched.filter(|(id, ..)| found.iter().all(|(seen, ..)| seen != id)) {
            found.push(hit);
        }
        if found.len() == MAX_DESCRIBED_TABLES {
            break;
        }
    }
    found
}

/// Column names from anything carrying a `fields[]` array — a metadata table
/// entry (when the instance inlines them) or a `/table/{id}/query_metadata`
/// payload.
/// `` `name`: col, col, … (+N more) ``, bounded.
fn render_columns(table: &str, alias: Option<&str>, columns: &[String]) -> String {
    let shown = columns.len().min(MAX_COLUMN_NAMES);
    let label = match alias {
        Some(a) => format!("`{table}` (alias `{a}`)"),
        None => format!("`{table}`"),
    };
    let list = columns[..shown].join(", ");
    match columns.len() - shown {
        0 => format!("{label}: {list}"),
        more => format!("{label}: {list} (+{more} more)"),
    }
}

/// Whether an extracted name is worth sorting the table list by.
///
/// Some phrasings put a word of the error template where the name should be —
/// `Table not found: userz` yields `not` — and sorting by that floats every
/// `notes`/`notifications` ahead of the real answer inside the name cap. The
/// list is still emitted; only the reordering is skipped.
fn plausible_name(name: &str) -> bool {
    const TEMPLATE_WORDS: &[&str] = &["not", "found", "table", "object", "relation", "column"];
    !name.is_empty() && !TEMPLATE_WORDS.contains(&name)
}

/// "Tables in database {db}: schema.name, … (+N more)" from the metadata
/// response, bounded by both the name and char caps. Empty when the
/// metadata carries no tables.
///
/// `missing` (the table the query asked for) floats its substring matches to
/// the front: with ~200 tables and a 60-name cap the real answer would often
/// fall outside the window otherwise. Not a similarity score — the correct
/// table is frequently a domain rename no string metric would rank.
fn table_list_suffix(database_id: i64, metadata: &Value, missing: Option<&str>) -> String {
    let mut names: Vec<String> = metadata
        .get("tables")
        .and_then(|v| v.as_array())
        .map(|tables| tables.iter().filter_map(qualified_table_name).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return String::new();
    }
    if let Some(missing) = missing.map(str::to_lowercase).filter(|m| plausible_name(m)) {
        let stem = missing.strip_suffix('s').unwrap_or(&missing);
        names.sort_by_key(|n| !n.to_lowercase().contains(stem));
    }
    let prefix = format!("Tables in database {database_id}: ");
    let mut listed = 0usize;
    let mut body = String::new();
    for name in names.iter().take(TABLE_LIST_MAX_NAMES) {
        let sep = if listed > 0 { ", " } else { "" };
        if prefix.len() + body.len() + sep.len() + name.len() > TABLE_LIST_MAX_CHARS {
            break;
        }
        body.push_str(sep);
        body.push_str(name);
        listed += 1;
    }
    let remaining = names.len() - listed;
    if remaining > 0 {
        format!("{prefix}{body} (+{remaining} more)")
    } else {
        format!("{prefix}{body}")
    }
}

/// How many places a misplaced table is pointed at, and how many other
/// databases are asked whether they hold it.
const TABLE_LOCATION_MAX_DATABASES: usize = 3;

/// Where the table a query could not find actually is, as this caller's own
/// key sees it: its database id, the table as that database spells it, and
/// the database's engine.
///
/// The instance's snapshot only says where to look. It is kept per site and
/// learns from every caller's responses, so on an install where people bring
/// their own Metabase keys it knows tables this caller may not be shown; a
/// name taken from it unchecked would tell them such a table exists and where.
/// This database is answered from `metadata`, which the caller's key just
/// fetched, and each other database the snapshot suggests is asked again with
/// that key — a few requests, spent only when a location is likely to come of
/// them, and each one saves the round the model would spend guessing.
async fn locate_for_caller(
    client: &MetabaseClient,
    database_id: i64,
    metadata: &Value,
    name: &str,
) -> Vec<(i64, String, Option<String>)> {
    if !plausible_name(&name.to_lowercase()) {
        return Vec::new();
    }
    let named = |metadata: &Value, database: i64| -> Vec<(i64, String, Option<String>)> {
        metadata
            .get("tables")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(qualified_table_name)
            .filter(|table| names_same_table(table, name))
            .map(|table| (database, table, client.cached_engine(database)))
            .collect()
    };
    let here = named(metadata, database_id);
    if !here.is_empty() {
        return here;
    }
    let mut suggested: Vec<i64> = client
        .locate_table(name)
        .into_iter()
        .map(|(database, ..)| database)
        .filter(|database| *database != database_id)
        .collect();
    suggested.dedup();
    let mut elsewhere = Vec::new();
    for database in suggested.into_iter().take(TABLE_LOCATION_MAX_DATABASES) {
        let Ok(theirs) = client
            .get(&format!("/database/{database}/metadata"), &[])
            .await
        else {
            continue;
        };
        elsewhere.extend(named(&theirs, database));
    }
    elsewhere
}

/// The sentence that sends a query to where its table is, given the places
/// [`locate_for_caller`] found it.
///
/// The table list alone answers "what is here"; measured on a live install,
/// about half the unknown-table errors were for a table that does exist, in
/// another of the instance's databases or under a schema the query did not
/// write, and the list of this database's tables sends the model guessing at
/// which. Empty when the name is nowhere found, or is a word of the error
/// template rather than a name.
fn table_location_hint(
    database_id: i64,
    name: &str,
    located: &[(i64, String, Option<String>)],
) -> String {
    if !plausible_name(&name.to_lowercase()) {
        return String::new();
    }
    let (here, elsewhere): (Vec<_>, Vec<_>) =
        located.iter().partition(|(db, ..)| *db == database_id);
    let spelled = |tables: &[&(i64, String, Option<String>)]| {
        let shown = tables
            .iter()
            .take(TABLE_LOCATION_MAX_DATABASES)
            .map(|(db, table, engine)| match (*db == database_id, engine) {
                (true, _) => format!("`{table}`"),
                (false, Some(engine)) => format!("database {db} (`{engine}`) as `{table}`"),
                (false, None) => format!("database {db} as `{table}`"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        match tables.len().saturating_sub(TABLE_LOCATION_MAX_DATABASES) {
            0 => shown,
            more => format!("{shown} (+{more} more)"),
        }
    };
    match (here.as_slice(), elsewhere.as_slice()) {
        // Here under the very name the query used: the database and its
        // metadata disagree, and no spelling this could offer would help.
        (here, _) if here.iter().any(|(_, table, _)| table == name) => String::new(),
        (here @ [_, ..], _) => format!(
            "Database {database_id} has this table as {} — write it exactly that way and resend.",
            spelled(here)
        ),
        ([], [only]) => format!(
            "`{name}` is not in database {database_id}. It is in {} — resend there, with that \
             database_id and in that engine's dialect.",
            spelled(&[only])
        ),
        ([], several @ [_, ..]) => format!(
            "`{name}` is not in database {database_id}. It exists in {} — resend to the one the \
             question is about, with its database_id and in its engine's dialect.",
            spelled(several)
        ),
        ([], []) => String::new(),
    }
}

/// The embedded query error Metabase wraps in an HTTP-202 success body, if
/// any: a non-empty `error` string means the SQL failed.
fn embedded_query_error(out: &Value) -> Option<&str> {
    out.get("error")
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
}

/// Convert an embedded native-query error into a real failure, naming the
/// engine when known. Success bodies pass through. Schema enrichment happens
/// once, in [`enrich_query_error`], so that an HTTP-400 and an embedded-202
/// failure are answered identically.
fn native_query_outcome(out: Value, database_id: i64, engine: Option<&str>) -> Result<Value> {
    let Some(err) = embedded_query_error(&out).map(str::to_string) else {
        return Ok(out);
    };
    Err(eyre::eyre!(match engine {
        Some(engine) =>
            format!("query failed on database {database_id} (engine `{engine}`): {err}"),
        None => format!("query failed on database {database_id}: {err}"),
    }))
}

/// Append the real schema to an error that amounts to "that name isn't real".
///
/// Fetches metadata only when the error classifies, so a syntax error costs no
/// extra round-trip, and fails open to the original message on any lookup
/// problem — a worse error is never worth failing the tool differently.
async fn enrich_query_error(
    error: eyre::Report,
    client: &Arc<MetabaseClient>,
    database_id: i64,
    sql: &str,
) -> eyre::Report {
    let text = error.to_string();
    let Some(schema_error) = classify_schema_error(&text) else {
        return error;
    };
    let Ok(metadata) = client
        .get(&format!("/database/{database_id}/metadata"), &[])
        .await
    else {
        return error;
    };
    // The response is already paid for; keep it so the next turn's tool schema
    // can carry these names instead of the model guessing them again.
    client.record_schema(database_id, &metadata);

    let hint = match &schema_error {
        SchemaError::UnknownTable { name } => [
            table_location_hint(
                database_id,
                name,
                &locate_for_caller(client, database_id, &metadata, name).await,
            ),
            table_list_suffix(database_id, &metadata, Some(name)),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"),
        SchemaError::UnknownColumn { qualifier, column } => {
            let (wanted, resolved) = tables_for_column(sql, qualifier.as_deref());
            let (described, complete) =
                describe_tables(client, database_id, &metadata, &wanted).await;
            let alias = resolved.then_some(qualifier.as_deref()).flatten();
            column_hint(column, alias, &described, complete)
        }
    };
    match hint.is_empty() {
        true => error,
        false => eyre::eyre!("{text}\n\n{hint}"),
    }
}

/// Fetch the columns of each referenced table, preferring the fields this
/// Metabase version may already have inlined in the database metadata over a
/// per-table round-trip. Tables that resolve to nothing are dropped.
async fn describe_tables(
    client: &Arc<MetabaseClient>,
    database_id: i64,
    metadata: &Value,
    wanted: &[String],
) -> (Vec<(String, Vec<String>)>, bool) {
    let mut expected: Vec<&String> = wanted.iter().collect();
    expected.dedup();
    let expected = expected.len().min(MAX_DESCRIBED_TABLES);
    let mut described = Vec::new();
    for (id, name, entry) in referenced_tables(metadata, wanted) {
        let mut columns = field_names(entry);
        if columns.is_empty() {
            let Ok(payload) = client
                .get(&format!("/table/{id}/query_metadata"), &[])
                .await
            else {
                continue;
            };
            columns = field_names(&payload);
        }
        if !columns.is_empty() {
            client.record_columns(database_id, &name, columns.clone());
            described.push((name, columns));
        }
    }
    let complete = described.len() == expected;
    (described, complete)
}

/// The hint for a column that doesn't exist, given the schema already fetched.
///
/// Pure, and separate from the fetching, so the part that actually decides what
/// the agent reads — one-table versus several, the alias label, the caps — is
/// directly testable.
fn column_hint(
    column: &str,
    alias: Option<&str>,
    described: &[(String, Vec<String>)],
    complete: bool,
) -> String {
    if described.is_empty() {
        return String::new();
    }
    // One table means one answer — but ONLY if it is the whole answer. When a
    // table we referenced could not be described, the list may simply not
    // contain the column, and "don't guess again" would be a lie.
    let (body, instruction) = match described {
        [(table, columns)] if complete => (
            format!("Columns in {}", render_columns(table, alias, columns)),
            format!(
                "`{column}` is not one of them — use a real column and resend, don't guess again."
            ),
        ),
        many => (
            format!(
                "`{column}` is not a column of any table this query references. Columns by table:\n  {}",
                many.iter()
                    .map(|(table, columns)| render_columns(table, None, columns))
                    .collect::<Vec<_>>()
                    .join("\n  ")
            ),
            "Pick the table you meant and use one of its real columns.".to_string(),
        ),
    };
    // Truncate the listing, never the instruction — the instruction is the
    // actionable half and it sits at the end.
    format!("{}\n\n{instruction}", truncate_listing(body))
}

fn truncate_listing(mut body: String) -> String {
    if body.len() > SCHEMA_HINT_MAX_BYTES {
        let mut end = SCHEMA_HINT_MAX_BYTES;
        while !body.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        body.truncate(end);
        body.push('…');
    }
    body
}

/// Post a dataset query and return a slimmed, wrapped response envelope.
async fn run_dataset_query(
    client: &Arc<MetabaseClient>,
    path: &str,
    body: Value,
    source: &'static str,
    extra: Value,
) -> Result<Value> {
    let resp = client.post(path, body).await?;
    let mut out = extra;
    if let Some(data) = resp.get("data") {
        out["data"] = slim_dataset_data(data, source);
    }
    if let Some(status) = resp.get("status") {
        out["status"] = status.clone();
    }
    if let Some(err) = resp.get("error") {
        out["error"] = err.clone();
    }
    Ok(out)
}

// ── metabase.card_list ──────────────────────────────────────────────

metabase_tool!(CardListTool);

#[async_trait]
impl Tool for CardListTool {
    fn name(&self) -> &str {
        "metabase.card_list"
    }
    fn description(&self) -> &str {
        "List saved questions (cards). Optionally filter by collection_id or archived state."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "collection_id": { "type": "integer", "description": "Filter by collection ID" },
                "archived": { "type": "boolean", "description": "Include archived cards (default: false)" },
                "limit": { "type": "integer", "description": "Maximum cards to return (default: 100)", "minimum": 1, "maximum": 500 },
                "instance": self.ctx.instance_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let filter = if input["archived"].as_bool().unwrap_or(false) {
                "archived"
            } else {
                "all"
            };
            let data = client.get("/card", &[("f", filter.to_string())]).await?;
            let limit = coerce_to_u64(&input["limit"]).unwrap_or(100).min(500) as usize;
            let collection_id = coerce_to_i64(&input["collection_id"]);
            let cards: Vec<Value> = data
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|c| match collection_id {
                    Some(cid) => c.get("collection_id").and_then(|v| v.as_i64()) == Some(cid),
                    None => true,
                })
                .take(limit)
                .map(|c| wrap_description(c, "metabase_card"))
                .collect();
            info!(count = cards.len(), "Listed Metabase cards");
            Ok(json!({ "count": cards.len(), "cards": cards }))
        })
        .await
    }
}

// ── metabase.card_get ───────────────────────────────────────────────

metabase_tool!(CardGetTool);

#[async_trait]
impl Tool for CardGetTool {
    fn name(&self) -> &str {
        "metabase.card_get"
    }
    fn description(&self) -> &str {
        "Get a single saved question (card) by ID."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Card ID" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id =
                coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id' parameter"))?;
            let card = client.get(&format!("/card/{id}"), &[]).await?;
            Ok(wrap_description(card, "metabase_card"))
        })
        .await
    }
}

// ── metabase.card_query ─────────────────────────────────────────────

metabase_tool!(CardQueryTool);

#[async_trait]
impl Tool for CardQueryTool {
    fn name(&self) -> &str {
        "metabase.card_query"
    }
    fn description(&self) -> &str {
        "Execute a saved question (card) and return its result rows. Row count is capped."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Card ID to execute" },
                "parameters": {
                    "type": "array",
                    "description": "Optional parameter values for parameterised questions",
                    "items": { "type": "object" }
                },
                "limit": {
                    "type": "integer",
                    "description": "Row cap for this call. Default 2000; explicit values allowed up to the instance max_query_rows (hard ceiling 50000) — large results stage as tabular_artifacts, not context."
                },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id =
                coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id' parameter"))?;
            let cap = clamp_limit(coerce_to_u64(&input["limit"]), client.max_query_rows);
            let parameters = input
                .get("parameters")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([]));
            let body = json!({
                "parameters": parameters,
                "constraints": { "max-results": cap, "max-results-bare-rows": cap },
            });
            run_dataset_query(
                &client,
                &format!("/card/{id}/query"),
                body,
                "metabase_card_query",
                json!({ "card_id": id, "row_cap": cap }),
            )
            .await
        })
        .await
    }
}

// ── metabase.card_create ────────────────────────────────────────────

metabase_tool!(CardCreateTool);

#[async_trait]
impl Tool for CardCreateTool {
    fn name(&self) -> &str {
        "metabase.card_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new saved question (card) from a dataset_query."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "collection_id": { "type": "integer" },
                "display": { "type": "string", "description": "Visualization type (e.g. 'table', 'bar', 'line')", "default": "table" },
                "dataset_query": { "type": "object", "description": "Metabase dataset_query payload" },
                "visualization_settings": { "type": "object", "default": {} },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["name", "dataset_query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let name = input["name"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'name'"))?
                .to_string();
            let dataset_query = input
                .get("dataset_query")
                .filter(|v| !v.is_null())
                .cloned()
                .ok_or_else(|| eyre::eyre!("Missing 'dataset_query'"))?;
            let display = input
                .get("display")
                .and_then(|v| v.as_str())
                .unwrap_or("table");
            let viz = input
                .get("visualization_settings")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let mut body = json!({
                "name": name,
                "dataset_query": dataset_query,
                "display": display,
                "visualization_settings": viz,
            });
            merge_fields(&mut body, &input, &["description", "collection_id"]);
            client.post("/card", body).await
        })
        .await
    }
}

// ── metabase.card_update ────────────────────────────────────────────

metabase_tool!(CardUpdateTool);

#[async_trait]
impl Tool for CardUpdateTool {
    fn name(&self) -> &str {
        "metabase.card_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing saved question (card). Include only fields to change."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "name": { "type": "string" },
                "description": { "type": "string" },
                "collection_id": { "type": "integer" },
                "display": { "type": "string" },
                "dataset_query": { "type": "object" },
                "visualization_settings": { "type": "object" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let mut body = json!({});
            merge_fields(
                &mut body,
                &input,
                &[
                    "name",
                    "description",
                    "collection_id",
                    "display",
                    "dataset_query",
                    "visualization_settings",
                ],
            );
            client.put(&format!("/card/{id}"), body).await
        })
        .await
    }
}

// ── metabase.card_archive ───────────────────────────────────────────

metabase_tool!(CardArchiveTool);

#[async_trait]
impl Tool for CardArchiveTool {
    fn name(&self) -> &str {
        "metabase.card_archive"
    }
    fn description(&self) -> &str {
        "MUTATING: archive or unarchive a card (sets the `archived` flag)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "archived": { "type": "boolean", "description": "true to archive, false to restore", "default": true },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let archived = input
                .get("archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            client
                .put(&format!("/card/{id}"), json!({ "archived": archived }))
                .await
        })
        .await
    }
}

// ── metabase.dashboard_list ─────────────────────────────────────────

metabase_tool!(DashboardListTool);

#[async_trait]
impl Tool for DashboardListTool {
    fn name(&self) -> &str {
        "metabase.dashboard_list"
    }
    fn description(&self) -> &str {
        "List dashboards, optionally filtered by collection."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "collection_id": { "type": "integer" },
                "limit": { "type": "integer", "default": 100, "minimum": 1, "maximum": 500 },
                "instance": self.ctx.instance_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut q: Vec<(&str, String)> = vec![("models", "dashboard".to_string())];
            if let Some(cid) = coerce_to_i64(&input["collection_id"]) {
                q.push(("filter_items_in_collection_id", cid.to_string()));
            }
            let data = client.get("/search", &q).await?;
            let limit = coerce_to_u64(&input["limit"]).unwrap_or(100).min(500) as usize;
            let dashboards: Vec<Value> = data
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .map(|d| wrap_description(d, "metabase_dashboard"))
                .collect();
            Ok(json!({ "count": dashboards.len(), "dashboards": dashboards }))
        })
        .await
    }
}

// ── metabase.dashboard_get ──────────────────────────────────────────

metabase_tool!(DashboardGetTool);

#[async_trait]
impl Tool for DashboardGetTool {
    fn name(&self) -> &str {
        "metabase.dashboard_get"
    }
    fn description(&self) -> &str {
        "Get a dashboard by ID (includes dashcards)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let dash = client.get(&format!("/dashboard/{id}"), &[]).await?;
            Ok(wrap_description(dash, "metabase_dashboard"))
        })
        .await
    }
}

// ── metabase.dashboard_query ────────────────────────────────────────

metabase_tool!(DashboardQueryTool);

#[async_trait]
impl Tool for DashboardQueryTool {
    fn name(&self) -> &str {
        "metabase.dashboard_query"
    }
    fn description(&self) -> &str {
        "Execute every card in a dashboard. Each card's rows are row-capped independently."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Dashboard ID" },
                "parameters": { "type": "array", "items": { "type": "object" } },
                "limit": { "type": "integer", "description": "Per-card row cap. Default 2000; explicit values allowed up to the instance max_query_rows (hard ceiling 50000)." },
                "max_cards": { "type": "integer", "description": "Max cards to execute (default 20)" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let cap = clamp_limit(coerce_to_u64(&input["limit"]), client.max_query_rows);
            let max_cards = coerce_to_u64(&input["max_cards"]).unwrap_or(20).min(50) as usize;
            let parameters = input
                .get("parameters")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([]));

            let dash = client.get(&format!("/dashboard/{id}"), &[]).await?;
            let dashcards: Vec<(i64, i64)> = dash
                .get("dashcards")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|dc| {
                    let dashcard_id = dc.get("id").and_then(|v| v.as_i64())?;
                    let card_id = dc.get("card_id").and_then(|v| v.as_i64())?;
                    Some((dashcard_id, card_id))
                })
                .take(max_cards)
                .collect();

            let mut results: Vec<Value> = Vec::with_capacity(dashcards.len());
            for (dashcard_id, card_id) in dashcards {
                let path = format!("/dashboard/{id}/dashcard/{dashcard_id}/card/{card_id}/query");
                let body = json!({
                    "parameters": parameters,
                    "constraints": { "max-results": cap, "max-results-bare-rows": cap },
                });
                let entry = match run_dataset_query(
                    &client,
                    &path,
                    body,
                    "metabase_dashboard_query",
                    json!({ "dashcard_id": dashcard_id, "card_id": card_id }),
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => json!({
                        "dashcard_id": dashcard_id,
                        "card_id": card_id,
                        "error": e.to_string(),
                    }),
                };
                results.push(entry);
            }
            Ok(json!({
                "dashboard_id": id,
                "row_cap": cap,
                "count": results.len(),
                "results": results,
            }))
        })
        .await
    }
}

// ── metabase.dashboard_create ───────────────────────────────────────

metabase_tool!(DashboardCreateTool);

#[async_trait]
impl Tool for DashboardCreateTool {
    fn name(&self) -> &str {
        "metabase.dashboard_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a new dashboard. Pass `dashcards` to lay out cards, \
         `parameters` for dashboard filters, and `tabs` for multi-tab layouts."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "collection_id": { "type": "integer" },
                "collection_position": { "type": "integer" },
                "dashcards": dashcards_schema(),
                "parameters": { "type": "array", "items": { "type": "object" } },
                "tabs": { "type": "array", "items": { "type": "object" } },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let name = input["name"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'name'"))?
                .to_string();
            let mut body = json!({ "name": name });
            merge_fields(&mut body, &input, DASHBOARD_WRITE_FIELDS);
            client.post("/dashboard", body).await
        })
        .await
    }
}

// ── metabase.dashboard_update ───────────────────────────────────────

metabase_tool!(DashboardUpdateTool);

#[async_trait]
impl Tool for DashboardUpdateTool {
    fn name(&self) -> &str {
        "metabase.dashboard_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update an existing dashboard. Include only fields to change. \
         `dashcards` REPLACES the entire card layout — to add or drop a single \
         card without rebuilding it, use dashboard_add_cards / dashboard_remove_card."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "name": { "type": "string" },
                "description": { "type": "string" },
                "collection_id": { "type": "integer" },
                "collection_position": { "type": "integer" },
                "dashcards": dashcards_schema(),
                "parameters": { "type": "array", "items": { "type": "object" } },
                "tabs": { "type": "array", "items": { "type": "object" } },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let mut body = json!({});
            merge_fields(&mut body, &input, DASHBOARD_WRITE_FIELDS);
            client.put(&format!("/dashboard/{id}"), body).await
        })
        .await
    }
}

// ── metabase.dashboard_archive ──────────────────────────────────────

metabase_tool!(DashboardArchiveTool);

#[async_trait]
impl Tool for DashboardArchiveTool {
    fn name(&self) -> &str {
        "metabase.dashboard_archive"
    }
    fn description(&self) -> &str {
        "MUTATING: archive or unarchive a dashboard."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "archived": { "type": "boolean", "default": true },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let archived = input
                .get("archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            client
                .put(&format!("/dashboard/{id}"), json!({ "archived": archived }))
                .await
        })
        .await
    }
}

// ── dashboard card layout (shared) ──────────────────────────────────

/// Fields `dashboard_create` / `dashboard_update` forward to the
/// Metabase `POST`/`PUT /dashboard` body. `dashcards` is a full-layout
/// replacement; the add/remove tools below wrap the read-merge-write so
/// callers don't have to.
const DASHBOARD_WRITE_FIELDS: &[&str] = &[
    "description",
    "collection_id",
    "collection_position",
    "dashcards",
    "parameters",
    "tabs",
];

const DEFAULT_CARD_WIDTH: i64 = 12;
const DEFAULT_CARD_HEIGHT: i64 = 8;

/// JSON-schema fragment for a `dashcards` array, shared by create/update.
fn dashcards_schema() -> Value {
    json!({
        "type": "array",
        "description": "Full card layout — REPLACES all existing dashcards.",
        "items": {
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Existing dashcard id; negative for a new card" },
                "card_id": { "type": "integer" },
                "row": { "type": "integer" },
                "col": { "type": "integer" },
                "size_x": { "type": "integer" },
                "size_y": { "type": "integer" },
                "series": { "type": "array" },
                "parameter_mappings": { "type": "array" },
                "visualization_settings": { "type": "object" },
            },
            "required": ["card_id"]
        }
    })
}

/// First grid row below every existing dashcard — where auto-placed
/// additions stack so they never overlap the current layout.
fn next_free_row(dashcards: &[Value]) -> i64 {
    dashcards
        .iter()
        .map(|dc| {
            let row = dc.get("row").and_then(Value::as_i64).unwrap_or(0);
            let height = dc.get("size_y").and_then(Value::as_i64).unwrap_or(0);
            row + height
        })
        .max()
        .unwrap_or(0)
}

/// Normalise caller card specs into full dashcard objects: assign
/// negative temp ids (Metabase's new-card convention), default the grid
/// size, and auto-stack any card lacking an explicit `row`+`col` below
/// the existing layout. Errors if a spec omits a numeric `card_id`.
fn prepare_added_cards(existing: &[Value], specs: &[Value]) -> Result<Vec<Value>> {
    let mut next_row = next_free_row(existing);
    specs
        .iter()
        .enumerate()
        .map(|(idx, spec)| {
            let card_id = spec
                .get("card_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| eyre::eyre!("each card requires a numeric 'card_id'"))?;
            let size_x = spec
                .get("size_x")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_CARD_WIDTH);
            let size_y = spec
                .get("size_y")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_CARD_HEIGHT);
            let row = spec.get("row").and_then(Value::as_i64).unwrap_or(next_row);
            let col = spec.get("col").and_then(Value::as_i64).unwrap_or(0);
            // Advance the auto-stack cursor past every card — explicit ones
            // included — so later positionless cards never overlap.
            next_row = next_row.max(row + size_y);
            let mut dashcard = json!({
                "id": -(idx as i64 + 1),
                "card_id": card_id,
                "row": row,
                "col": col,
                "size_x": size_x,
                "size_y": size_y,
            });
            merge_fields(
                &mut dashcard,
                spec,
                &["series", "parameter_mappings", "visualization_settings"],
            );
            Ok(dashcard)
        })
        .collect()
}

/// Existing dashcards with the one identified by `dashcard_id` removed.
fn drop_dashcard(existing: &[Value], dashcard_id: i64) -> Vec<Value> {
    existing
        .iter()
        .filter(|dc| dc.get("id").and_then(Value::as_i64) != Some(dashcard_id))
        .cloned()
        .collect()
}

/// Current dashcards array for a dashboard, fetched fresh for a
/// read-merge-write. Missing/non-array `dashcards` yields an empty Vec.
async fn fetch_dashcards(client: &MetabaseClient, dashboard_id: i64) -> Result<Vec<Value>> {
    let dash = client
        .get(&format!("/dashboard/{dashboard_id}"), &[])
        .await?;
    Ok(dash
        .get("dashcards")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

// ── metabase.dashboard_add_cards ────────────────────────────────────

metabase_tool!(DashboardAddCardsTool);

#[async_trait]
impl Tool for DashboardAddCardsTool {
    fn name(&self) -> &str {
        "metabase.dashboard_add_cards"
    }
    fn description(&self) -> &str {
        "MUTATING: append cards to a dashboard, preserving the existing layout. \
         Cards without an explicit row/col auto-stack below current cards."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Dashboard ID" },
                "cards": {
                    "type": "array",
                    "description": "Cards to add (each needs at least a card_id).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "card_id": { "type": "integer" },
                            "row": { "type": "integer" },
                            "col": { "type": "integer" },
                            "size_x": { "type": "integer" },
                            "size_y": { "type": "integer" },
                            "series": { "type": "array" },
                            "parameter_mappings": { "type": "array" },
                            "visualization_settings": { "type": "object" },
                        },
                        "required": ["card_id"]
                    }
                },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id", "cards"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let specs = input
                .get("cards")
                .and_then(|v| v.as_array())
                .filter(|c| !c.is_empty())
                .ok_or_else(|| eyre::eyre!("'cards' must be a non-empty array"))?;
            let existing = fetch_dashcards(&client, id).await?;
            let added = prepare_added_cards(&existing, specs)?;
            let dashcards: Vec<Value> = existing.into_iter().chain(added).collect();
            let updated = client
                .put(
                    &format!("/dashboard/{id}"),
                    json!({ "dashcards": dashcards }),
                )
                .await?;
            Ok(wrap_description(updated, "metabase_dashboard"))
        })
        .await
    }
}

// ── metabase.dashboard_remove_card ──────────────────────────────────

metabase_tool!(DashboardRemoveCardTool);

#[async_trait]
impl Tool for DashboardRemoveCardTool {
    fn name(&self) -> &str {
        "metabase.dashboard_remove_card"
    }
    fn description(&self) -> &str {
        "MUTATING: remove a single card from a dashboard by its dashcard id, \
         preserving the rest of the layout."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "Dashboard ID" },
                "dashcard_id": { "type": "integer", "description": "The dashcard's own id (not the card_id)" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id", "dashcard_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let dashcard_id = coerce_to_i64(&input["dashcard_id"])
                .ok_or_else(|| eyre::eyre!("Missing 'dashcard_id'"))?;
            let existing = fetch_dashcards(&client, id).await?;
            let kept = drop_dashcard(&existing, dashcard_id);
            if kept.len() == existing.len() {
                return Err(eyre::eyre!(
                    "dashcard {dashcard_id} not found on dashboard {id}"
                ));
            }
            let updated = client
                .put(&format!("/dashboard/{id}"), json!({ "dashcards": kept }))
                .await?;
            Ok(wrap_description(updated, "metabase_dashboard"))
        })
        .await
    }
}

// ── metabase.collection_list ────────────────────────────────────────

metabase_tool!(CollectionListTool);

#[async_trait]
impl Tool for CollectionListTool {
    fn name(&self) -> &str {
        "metabase.collection_list"
    }
    fn description(&self) -> &str {
        "List collections."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "archived": { "type": "boolean", "default": false },
                "instance": self.ctx.instance_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let mut q: Vec<(&str, String)> = Vec::new();
            if input["archived"].as_bool().unwrap_or(false) {
                q.push(("archived", "true".to_string()));
            }
            let data = client.get("/collection", &q).await?;
            let rows = data.as_array().cloned().unwrap_or_default();
            let wrapped: Vec<Value> = rows
                .into_iter()
                .map(|c| wrap_description(c, "metabase_collection"))
                .collect();
            Ok(json!({ "count": wrapped.len(), "collections": wrapped }))
        })
        .await
    }
}

// ── metabase.collection_get ─────────────────────────────────────────

metabase_tool!(CollectionGetTool);

#[async_trait]
impl Tool for CollectionGetTool {
    fn name(&self) -> &str {
        "metabase.collection_get"
    }
    fn description(&self) -> &str {
        "Get a collection by ID (or 'root')."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": ["integer", "string"], "description": "Collection ID or 'root'" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = id_as_path_segment(&input["id"])?;
            let data = client.get(&format!("/collection/{id}"), &[]).await?;
            Ok(wrap_description(data, "metabase_collection"))
        })
        .await
    }
}

// ── metabase.collection_items ───────────────────────────────────────

metabase_tool!(CollectionItemsTool);

#[async_trait]
impl Tool for CollectionItemsTool {
    fn name(&self) -> &str {
        "metabase.collection_items"
    }
    fn description(&self) -> &str {
        "List items (cards, dashboards, sub-collections) inside a collection."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": ["integer", "string"], "description": "Collection ID or 'root'" },
                "models": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["card", "dashboard", "collection", "dataset", "snippet"] },
                    "description": "Item types to include"
                },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = id_as_path_segment(&input["id"])?;
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(models) = input.get("models").and_then(|v| v.as_array()) {
                for s in models.iter().filter_map(|m| m.as_str()) {
                    q.push(("models", s.to_string()));
                }
            }
            client.get(&format!("/collection/{id}/items"), &q).await
        })
        .await
    }
}

// ── metabase.collection_create ──────────────────────────────────────

metabase_tool!(CollectionCreateTool);

#[async_trait]
impl Tool for CollectionCreateTool {
    fn name(&self) -> &str {
        "metabase.collection_create"
    }
    fn description(&self) -> &str {
        "MUTATING: create a collection."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "parent_id": { "type": "integer", "description": "Parent collection ID (omit for root)" },
                "color": { "type": "string", "description": "Hex colour (legacy Metabase versions)" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["name"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let name = input["name"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'name'"))?
                .to_string();
            let mut body = json!({ "name": name });
            merge_fields(&mut body, &input, &["description", "parent_id", "color"]);
            client.post("/collection", body).await
        })
        .await
    }
}

// ── metabase.collection_update ──────────────────────────────────────

metabase_tool!(CollectionUpdateTool);

#[async_trait]
impl Tool for CollectionUpdateTool {
    fn name(&self) -> &str {
        "metabase.collection_update"
    }
    fn description(&self) -> &str {
        "MUTATING: update a collection (name, description, parent, archive state)."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer" },
                "name": { "type": "string" },
                "description": { "type": "string" },
                "parent_id": { "type": "integer" },
                "archived": { "type": "boolean" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["id"]).ok_or_else(|| eyre::eyre!("Missing 'id'"))?;
            let mut body = json!({});
            merge_fields(
                &mut body,
                &input,
                &["name", "description", "parent_id", "archived"],
            );
            client.put(&format!("/collection/{id}"), body).await
        })
        .await
    }
}

// ── metabase.database_list ──────────────────────────────────────────

metabase_tool!(DatabaseListTool);

#[async_trait]
impl Tool for DatabaseListTool {
    fn name(&self) -> &str {
        "metabase.database_list"
    }
    fn description(&self) -> &str {
        "List all databases connected to this Metabase instance."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "instance": self.ctx.instance_prop(),
            }
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, _input| async move {
            let data = client.get("/database", &[]).await?;
            let rows = data
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| data.as_array().cloned())
                .unwrap_or_default();
            client.record_engines(&rows);
            Ok(json!({ "count": rows.len(), "databases": rows }))
        })
        .await
    }
}

// ── metabase.database_tables ────────────────────────────────────────

metabase_tool!(DatabaseTablesTool);

#[async_trait]
impl Tool for DatabaseTablesTool {
    fn name(&self) -> &str {
        "metabase.database_tables"
    }
    fn description(&self) -> &str {
        "List tables in a database via its metadata endpoint."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "database_id": { "type": "integer" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["database_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let db = coerce_to_i64(&input["database_id"])
                .ok_or_else(|| eyre::eyre!("Missing 'database_id'"))?;
            let data = client.get(&format!("/database/{db}/metadata"), &[]).await?;
            client.record_schema(db, &data);
            // Slim: just table id/name/schema — drop fields to keep context small.
            let tables = data
                .get("tables")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let slim: Vec<Value> = tables
                .into_iter()
                .map(|t| {
                    json!({
                        "id": t.get("id"),
                        "name": t.get("name"),
                        "schema": t.get("schema"),
                        "description": t.get("description"),
                    })
                })
                .collect();
            Ok(json!({ "database_id": db, "count": slim.len(), "tables": slim }))
        })
        .await
    }
}

// ── metabase.table_fields ───────────────────────────────────────────

metabase_tool!(TableFieldsTool);

#[async_trait]
impl Tool for TableFieldsTool {
    fn name(&self) -> &str {
        "metabase.table_fields"
    }
    fn description(&self) -> &str {
        "List fields (columns) for a table."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "table_id": { "type": "integer" },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["table_id"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let id = coerce_to_i64(&input["table_id"])
                .ok_or_else(|| eyre::eyre!("Missing 'table_id'"))?;
            let data = client
                .get(&format!("/table/{id}/query_metadata"), &[])
                .await?;
            // Keep what this already cost: the response names its own database
            // and table, so the columns can go straight into the tool schema
            // the next turn builds.
            if let (Some(db), Some(name)) = (
                data.get("db_id").and_then(|v| v.as_i64()),
                data.get("name").and_then(|v| v.as_str()),
            ) {
                let qualified = match data.get("schema").and_then(|v| v.as_str()) {
                    Some(sch) if !sch.is_empty() => format!("{sch}.{name}"),
                    _ => name.to_string(),
                };
                client.record_columns(db, &qualified, field_names(&data));
            }
            let fields = data
                .get("fields")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let slim: Vec<Value> = fields
                .into_iter()
                .map(|f| {
                    json!({
                        "id": f.get("id"),
                        "name": f.get("name"),
                        "display_name": f.get("display_name"),
                        "base_type": f.get("base_type"),
                        "semantic_type": f.get("semantic_type"),
                        "description": f.get("description"),
                    })
                })
                .collect();
            Ok(json!({ "table_id": id, "count": slim.len(), "fields": slim }))
        })
        .await
    }
}

// ── metabase.dataset_query_native ───────────────────────────────────

metabase_tool!(DatasetQueryNativeTool);

#[async_trait]
impl Tool for DatasetQueryNativeTool {
    fn name(&self) -> &str {
        "metabase.dataset_query_native"
    }
    fn description(&self) -> &str {
        "MUTATING (read-execute): run a raw SQL query against a Metabase database. Row-capped; respects database permissions of the API key. SQL dialect MUST match the target database's engine (check `engine` via metabase.database_list); e.g. ILIKE exists only on Postgres-family engines — on MySQL use LOWER(col) LIKE. The `database_id` description carries the real table and column names that have been learned so far — write your SQL from those, and do not invent a name that is not there. If a query still fails on an unknown column or table, the error lists the REAL names — fix the query from that list and resend; do not guess a second time."
    }
    fn input_schema(&self) -> Value {
        let description = self
            .ctx
            .clients
            .get(&self.ctx.default_instance)
            .and_then(|c| c.published_description())
            .map_or_else(
                || DATABASE_ID_STATIC_DESCRIPTION.to_string(),
                |d| d.to_string(),
            );
        json!({
            "type": "object",
            "properties": {
                "database_id": { "type": "integer", "description": description },
                "query": { "type": "string", "description": "SQL string" },
                "template_tags": { "type": "object", "description": "Optional Metabase template tags" },
                "parameters": { "type": "array", "items": { "type": "object" } },
                "limit": { "type": "integer", "description": "Row cap. Default 2000; explicit values allowed up to the instance max_query_rows (hard ceiling 50000) — large results stage as tabular_artifacts, not context." },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["database_id", "query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let db = coerce_to_i64(&input["database_id"])
                .ok_or_else(|| eyre::eyre!("Missing 'database_id'"))?;
            let query = input["query"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'query'"))?
                .to_string();
            // Dialect lint: allowlist-of-bad engines only; any lookup error
            // fails open to no lint.
            let engine = client.engine_for(db).await.ok().flatten();
            if let Some(engine) = engine.as_deref().filter(|e| ilike_rejected(&query, e)) {
                return Err(eyre::eyre!(
                    "dialect error: database {db} runs `{engine}`, which does not support ILIKE. \
                     Rewrite using LOWER(column) LIKE LOWER('%text%') and resend. The query was \
                     NOT executed."
                ));
            }
            let cap = clamp_limit(coerce_to_u64(&input["limit"]), client.max_query_rows);
            let mut native = json!({ "query": query });
            if let Some(tt) = input.get("template_tags").filter(|v| !v.is_null()) {
                native["template-tags"] = tt.clone();
            }
            let parameters = input
                .get("parameters")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([]));
            let body = json!({
                "database": db,
                "type": "native",
                "native": native,
                "parameters": parameters,
                "constraints": { "max-results": cap, "max-results-bare-rows": cap },
            });
            // Both failure shapes — a real HTTP 400 and an error embedded in
            // an HTTP-202 body — must end up as one `Err`, or enrichment
            // reaches only one of them (it previously reached only the 202).
            let outcome = run_dataset_query(
                &client,
                "/dataset",
                body,
                "metabase_native_query",
                json!({ "database_id": db, "row_cap": cap }),
            )
            .await
            .and_then(|out| native_query_outcome(out, db, engine.as_deref()));

            // Which tables this query reached for, and whether the reach was
            // wrong. Free: the parse is the one `enrich_query_error` already
            // does, and it is what ranks tables for the tool schema.
            let reached = referenced_table_names(&query);
            let fumbled = outcome
                .as_ref()
                .err()
                .map(|e| classify_schema_error(&e.to_string()).is_some())
                .unwrap_or(false);
            for table in &reached {
                client.note_table_use(db, table, fumbled);
            }

            match outcome {
                Ok(v) => Ok(v),
                Err(e) => Err(enrich_query_error(e, &client, db, &query).await),
            }
        })
        .await
    }
}

// ── metabase.search ─────────────────────────────────────────────────

metabase_tool!(SearchTool);

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "metabase.search"
    }
    fn description(&self) -> &str {
        "Global search across cards, dashboards, collections, etc."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search text" },
                "models": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Item types (card, dashboard, collection, dataset, table, database, segment, metric)"
                },
                "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 500 },
                "instance": self.ctx.instance_prop(),
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        run(&self.ctx, input, |client, input| async move {
            let query = input["query"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Missing 'query'"))?
                .to_string();
            let mut q: Vec<(&str, String)> = vec![("q", query)];
            if let Some(models) = input.get("models").and_then(|v| v.as_array()) {
                for s in models.iter().filter_map(|m| m.as_str()) {
                    q.push(("models", s.to_string()));
                }
            }
            let data = client.get("/search", &q).await?;
            let limit = coerce_to_u64(&input["limit"]).unwrap_or(50).min(500) as usize;
            let results: Vec<Value> = data
                .get("data")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(limit)
                .map(|r| wrap_description(r, "metabase_search"))
                .collect();
            Ok(json!({ "count": results.len(), "results": results }))
        })
        .await
    }
}

// ── helpers ─────────────────────────────────────────────────────────

/// Render an id param safely as a URL path segment. Accepts integer or the
/// string literal "root" (Metabase collections).
fn id_as_path_segment(v: &Value) -> Result<String> {
    if let Some(i) = v.as_i64() {
        return Ok(i.to_string());
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if s == "root" {
            return Ok("root".to_string());
        }
        if let Ok(n) = s.parse::<i64>() {
            return Ok(n.to_string());
        }
    }
    Err(eyre::eyre!("Expected integer ID or 'root', got: {}", v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_clients() -> Arc<HashMap<String, Arc<MetabaseClient>>> {
        Arc::new(HashMap::new())
    }

    #[test]
    fn instance_description_single() {
        let clients = empty_clients();
        let s = instance_description(&clients, "default");
        assert!(s.contains("multi-instance"));
    }

    #[test]
    fn instance_description_multi() {
        let mut map: HashMap<String, Arc<MetabaseClient>> = HashMap::new();
        map.insert(
            "prod".to_string(),
            Arc::new(MetabaseClient::new("mb_x", "https://mb.prod", 2000).unwrap()),
        );
        map.insert(
            "staging".to_string(),
            Arc::new(MetabaseClient::new("mb_y", "https://mb.staging", 2000).unwrap()),
        );
        let s = instance_description(&map, "prod");
        assert!(s.contains("prod"));
        assert!(s.contains("staging"));
        assert!(s.contains("Default: prod"));
        assert!(instance_description(&map, "gone").contains("Default: none — pass instance"));
    }

    #[test]
    fn resolve_client_explicit_unknown() {
        let clients = HashMap::new();
        let err = resolve_client(&clients, "default", &json!({ "instance": "ghost" }))
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("Unknown Metabase instance"));
    }

    #[test]
    fn resolve_client_honours_environment_lock() {
        use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};
        let mut clients: HashMap<String, Arc<MetabaseClient>> = HashMap::new();
        clients.insert(
            "prod".into(),
            Arc::new(MetabaseClient::new("k", "https://mb.prod", 2000).unwrap()),
        );
        clients.insert(
            "staging".into(),
            Arc::new(MetabaseClient::new("k", "https://mb.staging", 2000).unwrap()),
        );
        let pins: workforce_agent::llm::environment::SkillPins =
            [("metabase".to_string(), vec!["prod".to_string()])].into();
        ACTIVE_ENVIRONMENT.sync_scope("harbor-prod".to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, || {
                // No instance requested → pinned to prod even though default is staging.
                let c = resolve_client(&clients, "staging", &json!({})).unwrap();
                assert!(Arc::ptr_eq(&c, clients.get("prod").unwrap()));
                // A sideways switch to staging is refused as dormant.
                let err = resolve_client(&clients, "staging", &json!({ "instance": "staging" }))
                    .err()
                    .expect("expected dormant refusal");
                assert!(err.to_string().contains("dormant"));
            });
        });
    }

    #[test]
    fn id_as_path_segment_accepts_root_and_int() {
        assert_eq!(id_as_path_segment(&json!(42)).ok().unwrap(), "42");
        assert_eq!(id_as_path_segment(&json!("root")).ok().unwrap(), "root");
        assert_eq!(id_as_path_segment(&json!("12")).ok().unwrap(), "12");
        assert!(id_as_path_segment(&json!("bogus")).is_err());
    }

    #[test]
    fn wrap_description_wraps_nonempty() {
        let v = json!({ "description": "hello" });
        let wrapped = wrap_description(v, "metabase_card");
        let desc = wrapped["description"].as_str().unwrap();
        assert!(desc.contains("hello"));
        assert!(desc.len() > "hello".len()); // markers added
    }

    #[test]
    fn wrap_description_leaves_empty_alone() {
        let v = json!({ "description": "" });
        let wrapped = wrap_description(v, "metabase_card");
        assert_eq!(wrapped["description"], "");
    }

    #[test]
    fn slim_dataset_data_drops_raw_rows_and_wraps() {
        let data = json!({
            "rows": [[1,2],[3,4]],
            "cols": [{"name":"a"}],
            "native_form": {"query":"SELECT *"},
            "insights": [{"some":"data"}]
        });
        let slim = slim_dataset_data(&data, "metabase_test");
        assert!(
            slim.get("rows").is_none(),
            "raw rows must not leak into slimmed output"
        );
        assert!(slim.get("cols").is_some());
        assert_eq!(slim["row_count"], 2);
        let rows_wrapped = slim["rows_wrapped"]
            .as_str()
            .expect("rows_wrapped present and string");
        assert!(rows_wrapped.contains("1"));
        assert!(rows_wrapped.contains("metabase_test"));
        assert!(slim.get("native_form").is_none());
        assert!(slim.get("insights").is_none());
    }

    #[test]
    fn slim_dataset_data_omits_wrapped_when_no_rows() {
        let data = json!({ "cols": [] });
        let slim = slim_dataset_data(&data, "metabase_test");
        assert!(slim.get("rows_wrapped").is_none());
        assert!(slim.get("row_count").is_none());
    }

    #[test]
    fn slim_dataset_data_inlines_without_agent_scope() {
        // `CURRENT_AGENT_ID` is not scoped (and the global store may or may
        // not be installed by other tests) — even a very large row set must
        // inline because offloading needs both. This preserves
        // backwards-compatible behaviour outside an agent run.
        let rows: Vec<Value> = (0..1_000)
            .map(|i| json!([format!("value-{i}"), i, "padding".repeat(20)]))
            .collect();
        let data = json!({
            "cols": [{"name": "a"}, {"name": "b"}, {"name": "c"}],
            "rows": rows,
        });
        let slim = slim_dataset_data(&data, "metabase_no_store");
        assert!(
            slim.get("rows_wrapped").is_some(),
            "must inline without store"
        );
        assert!(slim.get("artifact_id").is_none());
    }

    #[test]
    fn slim_dataset_data_stages_canonical_objects_when_large() {
        use crate::adapters::test_support::fresh_agent_id;
        use workforce_agent::artifacts::{ArtifactBody, ArtifactId};

        let agent = fresh_agent_id("metabase-offload");
        let rows: Vec<Value> = (0..500)
            .map(|i| {
                json!([
                    format!("user-{i}@example.com"),
                    format!("First-{i}"),
                    i,
                    "padding".repeat(20)
                ])
            })
            .collect();
        let data = json!({
            "cols": [
                {"name": "email"},
                {"name": "email"},
                {"name": "id"},
                {"name": ""}
            ],
            "rows": rows,
        });

        let slim = workforce_agent::runner::CURRENT_AGENT_ID.sync_scope(agent.clone(), || {
            slim_dataset_data(&data, "metabase_offload_test")
        });

        assert_eq!(slim["kind"], "tabular_artifact");
        assert_eq!(slim["rows"], 500);
        assert_eq!(slim["columns"], json!(["email", "email_2", "id", "col_4"]));
        assert!(slim.get("rows_wrapped").is_none());
        assert!(slim.get("row_count").is_none());
        assert!(slim["note"].as_str().unwrap().contains("export_xlsx"));
        let preview = slim["preview"]
            .as_str()
            .expect("preview is a wrapped string");
        assert!(preview.contains("metabase_offload_test"));
        assert!(preview.contains("user-0@example.com"));

        let id = ArtifactId::parse(slim["artifact_id"].as_str().unwrap()).unwrap();
        let body = global_store().unwrap().read(&agent, &id).unwrap();
        let ArtifactBody::Json(Value::Array(stored)) = body else {
            panic!("expected canonical json rows");
        };
        assert_eq!(stored.len(), 500);
        assert_eq!(
            stored[0],
            json!({
                "email": "user-0@example.com",
                "email_2": "First-0",
                "id": 0,
                "col_4": "padding".repeat(20)
            })
        );
    }

    /// Every distinct shape captured from a production instance, where
    /// 61 of 62 failing turns were Metabase and ~45 of those were a guessed
    /// column name.
    #[test]
    fn classify_schema_error_over_the_real_production_errors() {
        let col = |q: Option<&str>, c: &str| {
            Some(SchemaError::UnknownColumn {
                qualifier: q.map(str::to_string),
                column: c.to_string(),
            })
        };
        let table = |n: &str| {
            Some(SchemaError::UnknownTable {
                name: n.to_string(),
            })
        };

        // MySQL, bare — the largest group.
        assert_eq!(
            classify_schema_error("Unknown column 'type' in 'field list'"),
            col(None, "type")
        );
        assert_eq!(
            classify_schema_error("Unknown column 'current_assignment_number' in 'field list'"),
            col(None, "current_assignment_number")
        );
        // MySQL, alias-qualified, across every clause seen in prod.
        assert_eq!(
            classify_schema_error("Unknown column 'r.hospital_id' in 'field list'"),
            col(Some("r"), "hospital_id")
        );
        assert_eq!(
            classify_schema_error("Unknown column 't.trashed_at' in 'where clause'"),
            col(Some("t"), "trashed_at")
        );
        assert_eq!(
            classify_schema_error("Unknown column 'd.hospital_id' in 'on clause'"),
            col(Some("d"), "hospital_id")
        );
        // MySQL missing table — the schema qualifier is dropped.
        assert_eq!(
            classify_schema_error("Table 'harbor.organisations' doesn't exist"),
            table("organisations")
        );
        // SQL Server shapes come free from the same markers.
        assert_eq!(
            classify_schema_error("Invalid column name 'hub_user_id'."),
            col(None, "hub_user_id")
        );
        assert_eq!(
            classify_schema_error("Invalid object name 'dbo.worker_patterns'."),
            table("worker_patterns")
        );

        // BigQuery phrases a missing table differently again; the old
        // `is_unknown_table_error` matched it and the classifier must too.
        // The project/dataset prefix is dropped, like MySQL's schema — the
        // bare name is what the table list is matched against.
        assert_eq!(
            classify_schema_error(
                "Not found: Table proj:ds.worker_patterns was not found in location EU"
            ),
            table("worker_patterns")
        );

        // Syntax errors carry no name to answer with.
        for syntax in [
            "You have an error in your SQL syntax; check the manual that corresponds to your \
             MySQL server version for the right syntax to use near 'div on div.id = d.division_id' \
             at line 4",
            "Missing 'database_id'",
        ] {
            assert_eq!(
                classify_schema_error(syntax),
                None,
                "not enrichable: {syntax}"
            );
        }
    }

    /// The regression this fix exists to prevent. Postgres reports a missing
    /// column with `does not exist` — the same phrase it uses for a missing
    /// relation — and the previous `is_unknown_table_error` matched that
    /// phrase alone, so a column error was answered with a table list.
    #[test]
    fn a_postgres_column_error_is_not_mistaken_for_a_missing_table() {
        assert_eq!(
            classify_schema_error("ERROR: column jp.deleted does not exist\n  Position: 275"),
            Some(SchemaError::UnknownColumn {
                qualifier: Some("jp".to_string()),
                column: "deleted".to_string(),
            })
        );
        assert_eq!(
            classify_schema_error("ERROR: relation \"worker_profiles\" does not exist"),
            Some(SchemaError::UnknownTable {
                name: "worker_profiles".to_string()
            })
        );
    }

    /// The classifier finds its index in a lowercased copy and slices the
    /// original, so the two must stay byte-aligned. `to_lowercase` does not
    /// guarantee that — `İ` lowercases to two chars — which orders the slice
    /// and reads the wrong name (or panics mid-codepoint). `to_ascii_lowercase`
    /// preserves length, and every marker is ASCII.
    #[test]
    fn a_non_ascii_error_prefix_does_not_order_the_identifier() {
        assert_eq!(
            classify_schema_error("İ Unknown column 'x' in 'field list'"),
            Some(SchemaError::UnknownColumn {
                qualifier: None,
                column: "x".to_string(),
            })
        );
        // Non-ASCII inside the identifier itself must survive intact.
        assert_eq!(
            classify_schema_error("Unknown column 'naïve_café' in 'field list'"),
            Some(SchemaError::UnknownColumn {
                qualifier: None,
                column: "naïve_café".to_string(),
            })
        );
    }

    /// Postgres names BOTH a column and a relation in one message, so the
    /// classifier's column-before-table ordering is what decides. Reversing it
    /// answers a missing-column error with a table list.
    #[test]
    fn a_message_naming_both_a_column_and_a_relation_classifies_as_the_column() {
        assert_eq!(
            classify_schema_error(
                "ERROR: column \"deleted\" of relation \"job_postings\" does not exist"
            ),
            Some(SchemaError::UnknownColumn {
                qualifier: None,
                column: "deleted".to_string(),
            })
        );
    }

    #[test]
    fn table_aliases_reads_from_and_join_clauses() {
        let sql = "SELECT r.id FROM rosters r \
                   JOIN departments AS d ON d.id = r.department_id \
                   LEFT JOIN `hospitals` h ON h.id = d.hospital_id \
                   JOIN harbor.orders ON orders.id = r.order_id";
        let refs = table_aliases(sql);
        assert_eq!(
            refs,
            vec![
                ("rosters".to_string(), Some("r".to_string())),
                ("departments".to_string(), Some("d".to_string())),
                ("hospitals".to_string(), Some("h".to_string())),
                ("orders".to_string(), None),
            ],
            "schema qualifier dropped, quoting stripped, AS optional, keyword not an alias"
        );
    }

    /// The stripper runs first for exactly this reason.
    #[test]
    fn table_aliases_ignores_the_word_from_inside_a_string_literal() {
        let sql = "SELECT 'copied from users' AS note FROM orders s WHERE s.id = 1";
        assert_eq!(
            table_aliases(sql),
            vec![("orders".to_string(), Some("s".to_string()))]
        );
    }

    #[test]
    fn table_aliases_ignores_tables_named_only_inside_comments() {
        let sql = "-- copied from audit_log\n\
                   SELECT 1 /* join legacy_orders here one day */ FROM orders s";
        assert_eq!(
            table_aliases(sql),
            vec![("orders".to_string(), Some("s".to_string()))],
            "a commented table is not a referenced table"
        );
    }

    #[test]
    fn strip_sql_noise_drops_comments_without_swallowing_code() {
        assert_eq!(
            strip_sql_noise("select a -- from b\nfrom c"),
            "select a \nfrom c"
        );
        assert_eq!(strip_sql_noise("select /* x */ 1"), "select   1");
        assert_eq!(
            strip_sql_noise("select '-- not a comment' from t"),
            "select '' from t",
            "a `--` inside a literal is not a comment"
        );
        assert_eq!(
            strip_sql_noise("-- don't include cancelled\nselect a from users u"),
            "\nselect a from users u",
            "an apostrophe inside a comment does not open a literal"
        );
        assert_eq!(strip_sql_noise("select 1 -- trailing"), "select 1 ");
    }

    /// A commented-out ILIKE never runs, so it must not be rejected as a
    /// dialect error — the lint used to see it.
    #[test]
    fn ilike_inside_a_comment_does_not_trip_the_dialect_lint() {
        assert!(!ilike_rejected(
            "select * from t where a = 1 -- don't use ILIKE on mysql",
            "mysql"
        ));
        assert!(!ilike_rejected(
            "select /* ILIKE is postgres-only */ * from t",
            "mysql"
        ));
        assert!(
            ilike_rejected("select * from t where a ILIKE 'x' -- real", "mysql"),
            "a real ILIKE outside the comment still trips it"
        );
    }

    /// A derived table's alias names the subquery, not the table inside it.
    /// Binding it would offer the base table's columns as the alias's.
    #[test]
    fn table_aliases_does_not_bind_a_subquery_alias_to_its_inner_table() {
        assert_eq!(
            table_aliases("SELECT o.total FROM (SELECT id, kind FROM orders) o"),
            vec![("orders".to_string(), None)],
            "`o` names the subquery, so `orders` gets no alias"
        );
    }

    #[test]
    fn table_aliases_reads_comma_joins_without_losing_tables() {
        assert_eq!(
            table_aliases("SELECT * FROM users, orders WHERE users.id = orders.user_id"),
            vec![("users".to_string(), None), ("orders".to_string(), None)],
            "a comma join is a table list, not a table and an alias"
        );
        assert_eq!(
            table_aliases("SELECT * FROM users u, orders o WHERE u.id = o.user_id"),
            vec![
                ("users".to_string(), Some("u".to_string())),
                ("orders".to_string(), Some("o".to_string()))
            ]
        );
    }

    #[test]
    fn table_aliases_strips_trailing_punctuation_and_inner_quotes() {
        assert_eq!(
            table_aliases("SELECT id, type FROM users;"),
            vec![("users".to_string(), None)],
            "a trailing semicolon is not part of the name"
        );
        assert_eq!(
            table_aliases("SELECT a FROM `db`.`users` u"),
            vec![("users".to_string(), Some("u".to_string()))],
            "the qualifier is split off before unquoting"
        );
        assert_eq!(
            table_aliases("SELECT a FROM [dbo].[orders] o ON 1=1"),
            vec![("orders".to_string(), Some("o".to_string()))]
        );
    }

    /// `Table not found: userz` puts a template word where the name belongs.
    /// The list must still emit, but must not be reordered by "not".
    #[test]
    fn an_implausible_extracted_name_does_not_reorder_the_table_list() {
        assert_eq!(
            classify_schema_error("Table not found: userz"),
            Some(SchemaError::UnknownTable {
                name: "not".to_string()
            }),
            "documents what this phrasing actually extracts"
        );
        let metadata = json!({ "tables": [
            { "id": 1, "name": "notifications" },
            { "id": 2, "name": "userz_real" },
        ]});
        let suffix = table_list_suffix(1, &metadata, Some("not"));
        assert!(suffix.contains("notifications") && suffix.contains("userz_real"));
        assert!(
            suffix.find("notifications").unwrap() < suffix.find("userz_real").unwrap(),
            "original order kept — no sort by a template word: {suffix}"
        );
        assert!(!plausible_name("not") && plausible_name("organisations"));
    }

    /// A CTE reads like a table but is not one, so it must not count against
    /// "did we describe everything" — which would soften wording that was
    /// correct.
    #[test]
    fn cte_names_are_not_counted_as_referenced_tables() {
        let sql = "WITH recent AS (SELECT id FROM orders) \
                   SELECT r.foo FROM recent r JOIN users u ON u.id = r.id";
        assert_eq!(cte_names(sql), vec!["recent".to_string()]);
        let (wanted, _) = tables_for_column(sql, None);
        assert!(!wanted.iter().any(|t| t == "recent"), "got {wanted:?}");
        assert!(wanted.contains(&"orders".to_string()) && wanted.contains(&"users".to_string()));
    }

    #[test]
    fn referenced_tables_describes_a_self_joined_table_once() {
        let metadata = json!({ "tables": [{ "id": 1, "name": "users", "fields": [] }] });
        let wanted = vec!["users".to_string(), "users".to_string()];
        assert_eq!(referenced_tables(&metadata, &wanted).len(), 1);
    }

    #[test]
    fn tables_for_column_resolves_an_alias_and_falls_back_to_everything() {
        let sql = "SELECT r.id FROM rosters r JOIN departments d ON d.id = r.department_id";
        assert_eq!(
            tables_for_column(sql, Some("r")),
            (vec!["rosters".to_string()], true),
            "a resolvable alias names one table"
        );
        assert_eq!(
            tables_for_column(sql, Some("zzz")),
            (
                vec!["rosters".to_string(), "departments".to_string()],
                false
            ),
            "an unresolvable qualifier falls back to every referenced table"
        );
        assert_eq!(
            tables_for_column(sql, None),
            (
                vec!["rosters".to_string(), "departments".to_string()],
                false
            ),
            "a bare column could belong to any of them"
        );
    }

    #[test]
    fn table_list_suffix_floats_substring_matches_to_the_front() {
        let metadata = json!({ "tables": [
            { "id": 1, "name": "orders" },
            { "id": 2, "name": "hospitals" },
            { "id": 3, "name": "organisation_staff_groups" },
            { "id": 4, "name": "organisation_registrations" },
        ]});
        let suffix = table_list_suffix(7, &metadata, Some("organisations"));
        let orgs = suffix.find("organisation_staff_groups").expect("listed");
        let orders = suffix.find("orders").expect("listed");
        assert!(orgs < orders, "stem matches lead: {suffix}");
        // Everything still gets listed — the real answer may be a rename.
        assert!(suffix.contains("hospitals"));
    }

    /// End-to-end over a mock Metabase: an HTTP-400 unknown-column error — the
    /// path EVERY production failure took, and the one the previous
    /// enrichment never reached — comes back carrying the real columns.
    #[tokio::test]
    async fn enrich_query_error_answers_a_400_unknown_column_with_the_real_columns() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/database/34/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables": [
                    { "id": 9, "name": "rosters",
                      "fields": [{ "name": "id" }, { "name": "department_id" }] },
                ]
            })))
            .mount(&server)
            .await;
        let client = Arc::new(MetabaseClient::new("k", &server.uri(), 2000).expect("client"));

        let enriched = enrich_query_error(
            eyre::eyre!("Metabase API error (400 Bad Request): Unknown column 'r.hospital_id' in 'field list'"),
            &client,
            34,
            "SELECT r.hospital_id FROM rosters r",
        )
        .await
        .to_string();

        assert!(
            enriched.contains("Unknown column 'r.hospital_id'"),
            "original preserved: {enriched}"
        );
        assert!(
            enriched.contains("`rosters` (alias `r`): id, department_id"),
            "{enriched}"
        );
        assert!(
            enriched.contains("use a real column and resend"),
            "{enriched}"
        );
    }

    fn located(entries: &[(i64, &str, Option<&str>)]) -> Vec<(i64, String, Option<String>)> {
        entries
            .iter()
            .map(|(db, table, engine)| (*db, table.to_string(), engine.map(str::to_string)))
            .collect()
    }

    #[test]
    fn an_unknown_table_is_pointed_at_the_database_that_has_it() {
        let hint = table_location_hint(
            3,
            "orders",
            &located(&[(7, "public.orders", Some("postgres")), (9, "orders", None)]),
        );
        assert_eq!(
            hint,
            "`orders` is not in database 3. It exists in database 7 (`postgres`) as \
             `public.orders`, database 9 as `orders` — resend to the one the question is about, \
             with its database_id and in its engine's dialect."
        );
    }

    #[test]
    fn a_table_held_by_one_other_database_is_sent_there() {
        let hint = table_location_hint(3, "orders", &located(&[(7, "orders", Some("mysql"))]));
        assert_eq!(
            hint,
            "`orders` is not in database 3. It is in database 7 (`mysql`) as `orders` — resend \
             there, with that database_id and in that engine's dialect."
        );
    }

    #[test]
    fn every_schema_of_this_database_holding_the_table_is_named() {
        let hint = table_location_hint(
            3,
            "orders",
            &located(&[(3, "catalogue.orders", None), (3, "archive.orders", None)]),
        );
        assert_eq!(
            hint,
            "Database 3 has this table as `catalogue.orders`, `archive.orders` — write it exactly \
             that way and resend."
        );
    }

    #[test]
    fn a_table_under_another_schema_of_this_database_is_given_its_qualified_name() {
        let hint = table_location_hint(
            3,
            "orders",
            &located(&[(3, "catalogue.orders", None), (7, "orders", None)]),
        );
        assert_eq!(
            hint,
            "Database 3 has this table as `catalogue.orders` — write it exactly that way and resend."
        );
    }

    #[test]
    fn a_table_known_nowhere_or_here_as_written_gets_no_location() {
        assert_eq!(table_location_hint(3, "orders", &[]), "");
        assert_eq!(
            table_location_hint(
                3,
                "orders",
                &located(&[(3, "orders", None), (7, "orders", None)])
            ),
            ""
        );
    }

    #[test]
    fn a_word_of_the_error_template_is_not_located_as_a_table() {
        assert_eq!(
            table_location_hint(3, "not", &located(&[(7, "not", None)])),
            ""
        );
    }

    #[test]
    fn a_misplaced_table_is_pointed_at_no_more_than_three_databases() {
        let hint = table_location_hint(
            1,
            "orders",
            &located(&[
                (2, "orders", None),
                (3, "orders", None),
                (4, "orders", None),
                (5, "orders", None),
            ]),
        );
        assert!(hint.contains("database 4 as `orders` (+1 more)"), "{hint}");
        assert!(!hint.contains("database 5"), "{hint}");
    }

    /// A Metabase whose databases answer `/metadata` with the given tables, as
    /// the calling key is shown them; a database not listed answers 403.
    async fn metabase_showing(databases: &[(i64, Value)]) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for (database, tables) in databases {
            Mock::given(method("GET"))
                .and(path(format!("/api/database/{database}/metadata")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "tables": tables })))
                .mount(&server)
                .await;
        }
        server
    }

    async fn unknown_orders_in_database_3(client: &Arc<MetabaseClient>) -> String {
        enrich_query_error(
            eyre::eyre!("Metabase API error (400 Bad Request): Table 'app.orders' doesn't exist"),
            client,
            3,
            "SELECT id FROM orders",
        )
        .await
        .to_string()
    }

    /// What some caller's key once saw in database 7: the site's snapshot
    /// keeps it, whoever asks next.
    fn snapshot_knowing_orders_in_database_7(server: &wiremock::MockServer) -> Arc<MetabaseClient> {
        let client = Arc::new(MetabaseClient::new("k", &server.uri(), 2000).expect("client"));
        client.record_schema(
            7,
            &json!({ "tables": [{ "id": 4, "name": "orders", "schema": "public" }] }),
        );
        client
    }

    /// End-to-end: the table lives in a database the instance already knows
    /// and this caller can see, so the error names that database ahead of
    /// this one's table list.
    #[tokio::test]
    async fn enrich_query_error_points_an_unknown_table_at_the_database_holding_it() {
        let server = metabase_showing(&[
            (3, json!([{ "id": 1, "name": "invoices" }])),
            (
                7,
                json!([{ "id": 4, "name": "orders", "schema": "public" }]),
            ),
        ])
        .await;
        let client = snapshot_knowing_orders_in_database_7(&server);

        let enriched = unknown_orders_in_database_3(&client).await;

        let location = enriched
            .find("It is in database 7 as `public.orders`")
            .unwrap_or_else(|| panic!("location named: {enriched}"));
        let list = enriched
            .find("Tables in database 3: invoices")
            .unwrap_or_else(|| panic!("table list kept: {enriched}"));
        assert!(location < list, "the location leads: {enriched}");
    }

    /// The snapshot is the site's, fed by every caller. A table it learned
    /// from someone else's key is not named to a caller whose own key is
    /// refused the database, or is shown it without that table.
    #[tokio::test]
    async fn a_table_the_callers_key_is_not_shown_is_not_located_for_them() {
        for theirs in [
            vec![(3, json!([{ "id": 1, "name": "invoices" }]))],
            vec![
                (3, json!([{ "id": 1, "name": "invoices" }])),
                (
                    7,
                    json!([{ "id": 5, "name": "catalogues", "schema": "public" }]),
                ),
            ],
        ] {
            let server = metabase_showing(&theirs).await;
            let client = snapshot_knowing_orders_in_database_7(&server);

            let enriched = unknown_orders_in_database_3(&client).await;

            assert!(!enriched.contains("database 7"), "{enriched}");
            assert!(!enriched.contains("public.orders"), "{enriched}");
            assert!(
                enriched.contains("Tables in database 3: invoices"),
                "{enriched}"
            );
        }
    }

    /// The schema the query should have written comes from what the caller's
    /// key was just shown of this database, with no further request.
    #[tokio::test]
    async fn a_table_under_another_schema_here_is_spelled_from_the_callers_own_fetch() {
        let server = metabase_showing(&[(
            3,
            json!([{ "id": 1, "name": "orders", "schema": "catalogue" }]),
        )])
        .await;
        let client = snapshot_knowing_orders_in_database_7(&server);

        let enriched = unknown_orders_in_database_3(&client).await;

        assert!(
            enriched.contains("Database 3 has this table as `catalogue.orders`"),
            "{enriched}"
        );
        assert!(!enriched.contains("database 7"), "{enriched}");
        let asked: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| request.url.path().to_string())
            .collect();
        assert_eq!(asked, ["/api/database/3/metadata"]);
    }

    /// The tool schema spells out columns keyed as the table list names the
    /// table. Columns learned from an error under the bare name would sit
    /// beside a table list that says `public.rosters` and never be ranked.
    #[tokio::test]
    async fn columns_learned_from_an_error_are_keyed_as_the_table_list_spells_the_table() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/database/34/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables": [{ "id": 9, "name": "rosters", "schema": "public" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/table/9/query_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "fields": [{ "name": "id" }, { "name": "department_id" }]
            })))
            .mount(&server)
            .await;
        let client = Arc::new(MetabaseClient::new("k", &server.uri(), 2000).expect("client"));

        let enriched = enrich_query_error(
            eyre::eyre!("ERROR: column r.hospital_id does not exist"),
            &client,
            34,
            "SELECT r.hospital_id FROM rosters r",
        )
        .await
        .to_string();

        assert!(
            enriched.contains("`public.rosters` (alias `r`): id, department_id"),
            "{enriched}"
        );
        let schema = client.database_schema(34);
        assert_eq!(schema.tables, ["public.rosters"]);
        assert_eq!(
            schema.columns.keys().collect::<Vec<_>>(),
            ["public.rosters"]
        );
    }

    /// A column error on an instance that does not inline `fields` must still
    /// answer, via the per-table endpoint `metabase.table_fields` uses.
    #[tokio::test]
    async fn enrich_query_error_falls_back_to_the_per_table_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/database/1/metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "tables": [{ "id": 9, "name": "users" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/table/9/query_metadata"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "fields": [{ "name": "id" }, { "name": "kind" }]
            })))
            .mount(&server)
            .await;
        let client = Arc::new(MetabaseClient::new("k", &server.uri(), 2000).expect("client"));

        let enriched = enrich_query_error(
            eyre::eyre!("Unknown column 'type' in 'field list'"),
            &client,
            1,
            "SELECT type FROM users",
        )
        .await
        .to_string();
        assert!(enriched.contains("`users`: id, kind"), "{enriched}");
    }

    /// A non-schema error must not cost a metadata round-trip, and an
    /// unreachable instance must not turn a real error into a worse one.
    #[tokio::test]
    async fn enrich_query_error_fails_open_and_skips_the_fetch_for_other_errors() {
        let unreachable =
            Arc::new(MetabaseClient::new("k", "http://127.0.0.1:1", 2000).expect("client"));

        // No mock is mounted at all: a syntax error must never reach the client.
        let syntax = enrich_query_error(
            eyre::eyre!(
                "Metabase API error (400 Bad Request): You have an error in your SQL syntax"
            ),
            &unreachable,
            1,
            "SELEC 1",
        )
        .await
        .to_string();
        assert_eq!(
            syntax,
            "Metabase API error (400 Bad Request): You have an error in your SQL syntax"
        );

        // A schema error whose lookup fails keeps the original message.
        let unenriched = enrich_query_error(
            eyre::eyre!("Unknown column 'type' in 'field list'"),
            &unreachable,
            1,
            "SELECT type FROM users",
        )
        .await
        .to_string();
        assert_eq!(unenriched, "Unknown column 'type' in 'field list'");
    }

    fn described(pairs: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        pairs
            .iter()
            .map(|(t, cols)| {
                (
                    t.to_string(),
                    cols.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    /// The alias resolved, so there is exactly one answer and the hint says so.
    #[test]
    fn column_hint_names_the_single_table_when_the_alias_resolved() {
        let hint = column_hint(
            "hospital_id",
            Some("r"),
            &described(&[("rosters", &["id", "name", "department_id"])]),
            true,
        );
        assert!(
            hint.contains("`rosters` (alias `r`): id, name, department_id"),
            "{hint}"
        );
        assert!(hint.contains("`hospital_id` is not one of them"), "{hint}");
        assert!(!hint.contains("Pick the table you meant"), "{hint}");
    }

    /// The common case (~28 of 45 in prod): a bare column against several
    /// tables. The agent still has to choose, and the wording must not present
    /// one list as the answer.
    #[test]
    fn column_hint_makes_the_ambiguity_explicit_for_a_bare_column() {
        let hint = column_hint(
            "type",
            None,
            &described(&[("users", &["id", "kind"]), ("orders", &["id", "status"])]),
            true,
        );
        assert!(
            hint.contains("`type` is not a column of any table this query references"),
            "{hint}"
        );
        assert!(hint.contains("Pick the table you meant"), "{hint}");
        for table in ["`users`: id, kind", "`orders`: id, status"] {
            assert!(hint.contains(table), "{hint}");
        }
        assert!(
            !hint.contains("alias"),
            "no alias resolved, so none claimed"
        );
    }

    /// A table we referenced but could not describe means the list may simply
    /// not contain the column — so the definitive wording must not be used.
    #[test]
    fn column_hint_softens_when_a_referenced_table_could_not_be_described() {
        let one = described(&[("users", &["id", "name"])]);
        let complete = column_hint("foo", None, &one, true);
        assert!(complete.contains("is not one of them"), "{complete}");

        let partial = column_hint("foo", None, &one, false);
        assert!(
            partial.contains("Pick the table you meant"),
            "an incomplete list must not claim the column is absent: {partial}"
        );
        assert!(!partial.contains("don't guess again"), "{partial}");
    }

    #[test]
    fn column_hint_is_empty_when_nothing_could_be_described() {
        assert_eq!(column_hint("x", None, &[], true), "");
    }

    /// Truncation must never eat the instruction — it is the actionable half
    /// and it sits at the end.
    #[test]
    fn column_hint_truncates_the_listing_but_keeps_the_instruction() {
        let wide: Vec<&str> = Vec::new();
        let _ = wide;
        let columns: Vec<String> = (0..MAX_COLUMN_NAMES)
            .map(|i| format!("a_very_long_column_name_number_{i:03}"))
            .collect();
        let hint = column_hint(
            "missing",
            None,
            &[
                ("t1".to_string(), columns.clone()),
                ("t2".to_string(), columns.clone()),
                ("t3".to_string(), columns),
            ],
            true,
        );
        assert!(hint.len() > SCHEMA_HINT_MAX_BYTES, "the listing overflowed");
        assert!(
            hint.ends_with("Pick the table you meant and use one of its real columns."),
            "instruction survives truncation: …{}",
            &hint[hint.len().saturating_sub(120)..]
        );
        assert!(hint.contains('…'), "the listing itself was elided");
    }

    #[test]
    fn referenced_tables_resolves_against_real_schema_before_capping() {
        let metadata = json!({ "tables": [
            { "id": 1, "name": "orders", "fields": [{ "name": "id" }] },
            { "id": 2, "name": "rosters" },
        ]});
        // Phantom names (a CTE, a mis-parse) must not consume the cap.
        let wanted: Vec<String> = ["cte_a", "cte_b", "cte_c", "cte_d", "orders", "rosters"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found = referenced_tables(&metadata, &wanted);
        assert_eq!(
            found
                .iter()
                .map(|(id, n, _)| (*id, n.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "orders"), (2, "rosters")],
            "unknown names are dropped, not counted against the cap"
        );
        // Inline fields are used when present; absent ones fall back to the
        // per-table fetch, which `describe_tables` does.
        assert_eq!(field_names(found[0].2), vec!["id".to_string()]);
        assert!(field_names(found[1].2).is_empty());
    }

    #[test]
    fn render_columns_labels_the_alias_and_caps_the_list() {
        let few = vec!["id".to_string(), "name".to_string()];
        assert_eq!(
            render_columns("rosters", Some("r"), &few),
            "`rosters` (alias `r`): id, name"
        );
        assert_eq!(render_columns("rosters", None, &few), "`rosters`: id, name");

        let many: Vec<String> = (0..MAX_COLUMN_NAMES + 5).map(|i| format!("c{i}")).collect();
        let rendered = render_columns("wide", None, &many);
        assert!(rendered.ends_with("(+5 more)"), "{rendered}");
    }

    #[test]
    fn truncate_listing_bounds_a_runaway_body_on_a_char_boundary() {
        // 3 bytes per char, and the cap is not a multiple of 3, so the
        // char-boundary walk actually has to move — a 2-byte char would land
        // on a boundary by luck and never exercise it.
        assert_ne!(
            SCHEMA_HINT_MAX_BYTES % 3,
            0,
            "the cap must not divide evenly"
        );
        let body = truncate_listing("あ".repeat(SCHEMA_HINT_MAX_BYTES));
        assert!(body.len() <= SCHEMA_HINT_MAX_BYTES + '…'.len_utf8());
        assert!(body.ends_with('…'));
        assert_eq!(
            truncate_listing("fine".to_string()),
            "fine",
            "under the cap is untouched"
        );
    }

    #[test]
    fn strip_sql_noise_blanks_literals_and_handles_escapes() {
        assert_eq!(
            strip_sql_noise("select * from t where a = 'x ILIKE y'"),
            "select * from t where a = ''"
        );
        assert_eq!(
            strip_sql_noise("select 'it''s ILIKE here' from t"),
            "select '' from t"
        );
        assert_eq!(
            strip_sql_noise("select 1 where a = 'unterminated ILIKE"),
            "select 1 where a = '",
            "unterminated literal blanks to end"
        );
        assert_eq!(strip_sql_noise("no literals"), "no literals");
    }

    #[test]
    fn ilike_rejected_matrix() {
        let q = "SELECT * FROM users WHERE name ILIKE '%bob%'";
        assert!(ilike_rejected(q, "mysql"));
        assert!(ilike_rejected(q, "mariadb"));
        assert!(ilike_rejected(q, "sqlserver"));
        assert!(ilike_rejected(q, "bigquery-cloud-sdk"));
        assert!(ilike_rejected(q, "MySQL"), "engine match case-insensitive");
        assert!(!ilike_rejected(q, "postgres"), "postgres supports ILIKE");
        assert!(!ilike_rejected(q, "redshift"), "unknown engine fails open");
        assert!(
            ilike_rejected("select 1 where a ilike b", "mysql"),
            "keyword match case-insensitive"
        );
        assert!(
            !ilike_rejected("SELECT * FROM t WHERE note = 'use ILIKE here'", "mysql"),
            "literal-only mention is stripped"
        );
        assert!(
            !ilike_rejected("SELECT MILIKE FROM t", "mysql"),
            "word boundary — MILIKE is not ILIKE"
        );
        assert!(
            !ilike_rejected("SELECT ilike_score FROM t", "mysql"),
            "identifier with underscore suffix is one token"
        );
        assert!(!ilike_rejected(
            "SELECT lower(a) LIKE lower(b) FROM t",
            "mysql"
        ));
    }

    #[test]
    fn unknown_table_shapes_classify_across_engines() {
        for (error, expected) in [
            ("Table 'app.userz' doesn't exist", "userz"),
            ("relation \"userz\" does not exist", "userz"),
            ("Invalid object name 'Userz'", "Userz"),
            ("Unknown table 'userz'", "userz"),
        ] {
            assert_eq!(
                classify_schema_error(error),
                Some(SchemaError::UnknownTable {
                    name: expected.to_string()
                }),
                "{error}"
            );
        }
        assert_eq!(classify_schema_error("syntax error at or near ILIKE"), None);
        assert_eq!(classify_schema_error(""), None);
    }

    fn metadata_with_tables(names: &[(&str, &str)]) -> Value {
        json!({
            "tables": names
                .iter()
                .map(|(schema, name)| json!({"schema": schema, "name": name}))
                .collect::<Vec<_>>()
        })
    }

    #[test]
    fn table_list_suffix_lists_schema_qualified_names() {
        let meta = metadata_with_tables(&[("public", "users"), ("", "orders")]);
        assert_eq!(
            table_list_suffix(5, &meta, None),
            "Tables in database 5: public.users, orders"
        );
        assert_eq!(table_list_suffix(5, &json!({"tables": []}), None), "");
        assert_eq!(table_list_suffix(5, &json!({}), None), "");
    }

    #[test]
    fn table_list_suffix_bounds_names_and_chars() {
        // Name cap: 100 tables → 60 listed, +40 more.
        let many: Vec<(String, String)> = (0..100)
            .map(|i| ("s".to_string(), format!("t{i:03}")))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(s, n)| (s.as_str(), n.as_str())).collect();
        let suffix = table_list_suffix(1, &metadata_with_tables(&refs), None);
        assert!(suffix.ends_with("(+40 more)"), "{suffix}");
        assert!(suffix.contains("s.t059"));
        assert!(!suffix.contains("s.t060"));

        // Char cap: long names hit 2_000 chars before the name cap.
        let long: Vec<(String, String)> = (0..40)
            .map(|i| {
                (
                    "schema".to_string(),
                    format!("very_long_table_name_{i:02}_{}", "x".repeat(60)),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = long.iter().map(|(s, n)| (s.as_str(), n.as_str())).collect();
        let suffix = table_list_suffix(1, &metadata_with_tables(&refs), None);
        assert!(suffix.len() <= TABLE_LIST_MAX_CHARS + " (+99 more)".len());
        assert!(suffix.contains("more)"), "char cap elides: {suffix}");
    }

    #[test]
    fn native_query_outcome_passes_success_through() {
        let out = json!({"database_id": 5, "data": {"cols": []}, "status": "completed"});
        let passed = native_query_outcome(out.clone(), 5, Some("mysql")).unwrap();
        assert_eq!(passed, out);

        let empty_error = json!({"database_id": 5, "error": ""});
        assert!(native_query_outcome(empty_error, 5, None).is_ok());
    }

    #[test]
    fn native_query_outcome_converts_embedded_errors_to_failures() {
        let out = json!({"database_id": 2, "status": "failed", "error": "You have an error in your SQL syntax"});
        let err = native_query_outcome(out, 2, Some("mysql")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "query failed on database 2 (engine `mysql`): You have an error in your SQL syntax"
        );

        let out = json!({"error": "boom"});
        let err = native_query_outcome(out, 3, None).unwrap_err();
        assert_eq!(err.to_string(), "query failed on database 3: boom");
    }

    #[test]
    fn referenced_table_names_skips_ctes_and_dedupes() {
        let sql = "WITH recent AS (SELECT 1) \
                   SELECT * FROM people p JOIN orders s ON s.person_id = p.id \
                   JOIN people p2 ON p2.id = s.covered_by JOIN recent r ON r.x = p.id";
        let names = referenced_table_names(sql);
        assert!(names.contains(&"people".to_string()), "{names:?}");
        assert!(names.contains(&"orders".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.eq_ignore_ascii_case("recent")),
            "a CTE is not a table: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| *n == "people").count(),
            1,
            "deduped: {names:?}"
        );
    }

    #[test]
    fn native_tool_description_names_the_dialect_contract() {
        let tool = DatasetQueryNativeTool::new(empty_clients(), "default".to_string());
        assert!(tool
            .description()
            .contains("SQL dialect MUST match the target database's engine"));
        let schema = tool.input_schema();
        assert_eq!(
            schema["properties"]["database_id"]["description"], DATABASE_ID_STATIC_DESCRIPTION,
            "no cached engines → static sentence"
        );
    }

    #[test]
    fn the_native_tool_schema_reads_only_the_published_description() {
        let client = Arc::new(MetabaseClient::new("k", "https://mb.example.com", 2000).unwrap());
        client.record_schema(
            2,
            &json!({"tables": [{"name": "people", "fields": [{"name": "id"}]}]}),
        );
        let clients = Arc::new(HashMap::from([(
            "default".to_string(),
            Arc::clone(&client),
        )]));
        let tool = DatasetQueryNativeTool::new(clients, "default".to_string());
        let description = |tool: &DatasetQueryNativeTool| {
            tool.input_schema()["properties"]["database_id"]["description"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            description(&tool),
            DATABASE_ID_STATIC_DESCRIPTION,
            "live learning stays out of the schema until it is published"
        );
        client.publish();
        assert!(description(&tool).contains("Known schema"));
    }

    #[test]
    fn merge_fields_copies_non_null_and_skips_null() {
        let mut body = json!({});
        let input = json!({
            "name": "keep",
            "description": null,
            "other": "ignored",
        });
        merge_fields(&mut body, &input, &["name", "description", "missing"]);
        assert_eq!(body["name"], "keep");
        assert!(body.get("description").is_none());
        assert!(body.get("missing").is_none());
        assert!(body.get("other").is_none());
    }

    #[test]
    fn next_free_row_is_bottom_of_layout() {
        assert_eq!(next_free_row(&[]), 0);
        let existing = vec![
            json!({ "row": 0, "size_y": 4 }),
            json!({ "row": 4, "size_y": 6 }),
        ];
        assert_eq!(next_free_row(&existing), 10);
    }

    #[test]
    fn prepare_added_cards_auto_stacks_below_existing() {
        let existing =
            vec![json!({ "id": 1, "card_id": 7, "row": 0, "col": 0, "size_x": 12, "size_y": 5 })];
        let added = prepare_added_cards(
            &existing,
            &[json!({ "card_id": 9 }), json!({ "card_id": 10 })],
        )
        .expect("prepare");
        assert_eq!(added.len(), 2);
        // Negative temp ids, distinct.
        assert_eq!(added[0]["id"], -1);
        assert_eq!(added[1]["id"], -2);
        // First stacks at the existing bottom (row 5), default-sized.
        assert_eq!(added[0]["row"], 5);
        assert_eq!(added[0]["col"], 0);
        assert_eq!(added[0]["size_x"], DEFAULT_CARD_WIDTH);
        assert_eq!(added[0]["size_y"], DEFAULT_CARD_HEIGHT);
        // Second stacks below the first (5 + default height).
        assert_eq!(added[1]["row"], 5 + DEFAULT_CARD_HEIGHT);
    }

    #[test]
    fn prepare_added_cards_respects_explicit_position_and_extras() {
        let added = prepare_added_cards(
            &[],
            &[json!({
                "card_id": 3,
                "row": 2,
                "col": 6,
                "size_x": 8,
                "size_y": 4,
                "series": [{ "id": 99 }],
                "visualization_settings": { "title": "x" },
            })],
        )
        .expect("prepare");
        assert_eq!(added[0]["row"], 2);
        assert_eq!(added[0]["col"], 6);
        assert_eq!(added[0]["size_x"], 8);
        assert_eq!(added[0]["series"][0]["id"], 99);
        assert_eq!(added[0]["visualization_settings"]["title"], "x");
        assert!(added[0].get("parameter_mappings").is_none());
    }

    #[test]
    fn prepare_added_cards_stacks_below_explicit_placements() {
        // An explicitly-placed tall card must push the auto-stack cursor
        // down so a following positionless card doesn't overlap it.
        let added = prepare_added_cards(
            &[],
            &[
                json!({ "card_id": 1, "row": 100, "size_y": 50 }),
                json!({ "card_id": 2 }),
            ],
        )
        .expect("prepare");
        assert_eq!(added[0]["row"], 100);
        assert_eq!(added[1]["row"], 150);
    }

    #[test]
    fn prepare_added_cards_honours_partial_position() {
        // col-only: the provided col is kept, row auto-stacks (not dropped).
        let added =
            prepare_added_cards(&[], &[json!({ "card_id": 1, "col": 9 })]).expect("prepare");
        assert_eq!(added[0]["col"], 9);
        assert_eq!(added[0]["row"], 0);
    }

    #[test]
    fn prepare_added_cards_requires_card_id() {
        let err = prepare_added_cards(&[], &[json!({ "row": 0, "col": 0 })]).unwrap_err();
        assert!(err.to_string().contains("card_id"));
    }

    #[test]
    fn drop_dashcard_removes_only_the_match() {
        let existing = vec![
            json!({ "id": 1, "card_id": 7 }),
            json!({ "id": 2, "card_id": 8 }),
        ];
        let kept = drop_dashcard(&existing, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["id"], 2);
        // No match leaves the layout untouched (caller treats this as a miss).
        assert_eq!(drop_dashcard(&existing, 99).len(), existing.len());
    }
}
