//! `know_how`: ask an agent what it knows about where its data and code
//! live, and teach it.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::settings::KnowHowConfig;

use super::{location_key, screen_intent, Access, Entry, Kind, KnowHow, Observation, Sites};

pub const NAME: &str = "know_how";

/// The agent's know-how config as of now (hot-reloaded), `None` when off.
pub type ConfigFn = Arc<dyn Fn() -> Option<KnowHowConfig> + Send + Sync>;
/// What the person in scope can reach.
pub type AccessFn = Arc<dyn Fn() -> Access + Send + Sync>;
/// Whether the person in scope is an admin.
pub type AdminFn = Arc<dyn Fn() -> bool + Send + Sync>;
/// Whether the conversation in scope has several people in it (unknown
/// counts as yes).
pub type SharedFn = Arc<dyn Fn() -> bool + Send + Sync>;

pub struct KnowHowTool {
    know_how: Arc<KnowHow>,
    agent: String,
    config: ConfigFn,
    access: AccessFn,
    is_admin: AdminFn,
    shared: SharedFn,
}

impl KnowHowTool {
    pub fn new(
        know_how: Arc<KnowHow>,
        agent: &str,
        config: ConfigFn,
        access: AccessFn,
        is_admin: AdminFn,
        shared: SharedFn,
    ) -> Self {
        Self {
            know_how,
            agent: agent.to_string(),
            config,
            access,
            is_admin,
            shared,
        }
    }
}

#[derive(Deserialize)]
struct Input {
    action: Option<String>,
    query: Option<String>,
    table: Option<String>,
    database_id: Option<i64>,
    tables: Option<Vec<String>>,
    columns: Option<Vec<String>>,
    instance: Option<String>,
    about: Option<String>,
    note: Option<String>,
    id: Option<i64>,
}

/// An entry as anyone may see it: its names, never who, how many or when.
fn describe(e: &Entry) -> serde_json::Value {
    let status = if e.pinned { "pinned" } else { "learned" };
    match e.kind {
        Kind::CodeLocation => serde_json::json!({
            "id": e.id,
            "kind": e.kind.as_str(),
            "repo": e.repo,
            "path": e.path,
            "symbol": e.symbol,
            "status": status,
        }),
        Kind::DataLocation | Kind::QueryShape | Kind::SchemaCorrection => serde_json::json!({
            "id": e.id,
            "kind": e.kind.as_str(),
            "database_id": e.database_id,
            "tables": e.tables,
            "columns": super::display_columns(e),
            "query_shape": e.shape,
            "does_not_exist": e.wrong,
            "note": e.note,
            "status": status,
        }),
    }
}

/// Every name an entry holds, for matching by words.
/// Whether the turn in progress was asked by `workforce eval`.
fn asked_by_an_eval() -> bool {
    workforce_agent::runner::CURRENT_SESSION_KEY
        .try_with(|channel| crate::turn_extraction::is_eval_channel(channel))
        .unwrap_or(false)
}

fn names(e: &Entry) -> String {
    [
        e.tables.join(" "),
        e.columns.join(" "),
        e.wrong.clone().unwrap_or_default(),
        e.repo.clone().unwrap_or_default(),
        e.path.clone().unwrap_or_default(),
        e.symbol.clone().unwrap_or_default(),
    ]
    .join(" ")
    .to_lowercase()
}

#[async_trait]
impl Tool for KnowHowTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "What this agent has learned about where data and code live, shared across everyone who uses \
         it: which database tables and columns answered which kinds of question, query shapes that \
         worked (values masked), names that do not exist and the real ones to use, and which files \
         and symbols of which repos answered code questions. action='search' with `query` finds what is known about a topic \
         ('what do you know about order delivery times?'); 'list' returns everything, optionally for one \
         `table`; 'add' teaches a location the user tells you (`database_id`, `tables`, optional \
         `columns`, and `about`: a generic description of what it answers, with no names or numbers). \
         An admin's addition is used at once; anyone else's is used for them, and for everyone once an \
         admin pins it. Admins: 'pending' lists additions awaiting review, 'pin' and 'remove' take an \
         `id`. Only names are kept, never values or anyone's messages."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["search", "list", "add", "pending", "pin", "remove"] },
                "query": { "type": "string", "description": "search: the topic or question." },
                "table": { "type": "string", "description": "list: only entries naming this table, repo or path." },
                "database_id": { "type": "integer", "description": "add: the Metabase database id." },
                "tables": { "type": "array", "items": { "type": "string" }, "description": "add: table names as the schema has them." },
                "columns": { "type": "array", "items": { "type": "string" }, "description": "add: the columns that matter." },
                "instance": { "type": "string", "description": "add: the Metabase instance, when there are several." },
                "about": { "type": "string", "description": "add: what it answers, generically ('order delivery time by month'); no names, numbers or dates." },
                "note": { "type": "string", "description": "add, admins only: a short description shown with the entry." },
                "id": { "type": "integer", "description": "pin / remove: the entry id." }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let input: Input = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(format!("Invalid input: {e}"), ms())),
        };
        let Some(cfg) = (self.config)() else {
            return Ok(ToolResult::failure(
                "Know-how is not enabled for this agent.",
                ms(),
            ));
        };
        // A linked person only, as on the turn path: anything else could pose
        // as a second confirmer.
        let Some(identity) = crate::user_config::current_identity_id()
            .as_deref()
            .and_then(crate::endpoint_cmd::identity_id)
        else {
            return Ok(ToolResult::failure(
                "know_how needs a signed-in person; refusing to run unscoped",
                ms(),
            ));
        };
        let access = (self.access)();
        let shared = (self.shared)();
        let agent = self.agent.as_str();
        let kh = &self.know_how;
        let entries = |list: Vec<Entry>| -> serde_json::Value {
            let list: Vec<serde_json::Value> = list.iter().map(describe).collect();
            serde_json::json!({ "count": list.len(), "entries": list })
        };
        match input.action.as_deref().unwrap_or("search") {
            "search" => {
                let query = input.query.unwrap_or_default();
                let mut found: Vec<Entry> = kh
                    .matches(agent, &cfg, &identity, &query, &access, shared)
                    .await?
                    .into_iter()
                    .map(|(e, _)| e)
                    .collect();
                if found.is_empty() {
                    let words: Vec<String> = query
                        .to_lowercase()
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .filter(|w| w.len() > 2)
                        .map(String::from)
                        .collect();
                    found = kh
                        .visible(agent, &cfg, &identity, &access, shared)
                        .await?
                        .into_iter()
                        .filter(|e| {
                            let names = names(e);
                            words.iter().any(|w| names.contains(w.as_str()))
                        })
                        .take(10)
                        .collect();
                }
                Ok(ToolResult::success(entries(found), ms()))
            }
            "list" => {
                let table = input.table.map(|t| t.to_lowercase());
                let list: Vec<Entry> = kh
                    .visible(agent, &cfg, &identity, &access, shared)
                    .await?
                    .into_iter()
                    .filter(|e| table.as_deref().is_none_or(|t| names(e).contains(t)))
                    .take(50)
                    .collect();
                Ok(ToolResult::success(entries(list), ms()))
            }
            "add" | "pending" | "pin" | "remove" if cfg.read_only() => Ok(ToolResult::failure(
                "This agent is a test copy reading another agent's know-how, which it never \
                 changes: add and review it on that agent.",
                ms(),
            )),
            "add" if asked_by_an_eval() => Ok(ToolResult::failure(
                "An eval's turn adds no know-how: a set's question is not what anyone asked.",
                ms(),
            )),
            "add" => self.add(input, &identity, &access.sites, ms).await,
            "pending" => {
                if !(self.is_admin)() {
                    return Ok(ToolResult::failure(
                        "Only an admin can review additions.",
                        ms(),
                    ));
                }
                let list: Vec<Entry> = kh
                    .store()
                    .entries(agent)
                    .await?
                    .into_iter()
                    .filter(|e| e.manual && !e.pinned && !e.stale && access.reaches(agent, e))
                    .collect();
                Ok(ToolResult::success(entries(list), ms()))
            }
            action @ ("pin" | "remove") => {
                let refused = || {
                    ToolResult::failure(
                        format!("No know-how entry with that id that you can {action}."),
                        ms(),
                    )
                };
                if !(self.is_admin)() {
                    return Ok(refused());
                }
                let Some(id) = input.id else {
                    return Ok(ToolResult::failure(format!("{action} needs id"), ms()));
                };
                // Only what the admin could reach anyway, and a pin only what
                // others already see or an addition awaiting review: never an
                // entry one person produced in private.
                let reaches = kh
                    .store()
                    .entries(agent)
                    .await?
                    .iter()
                    .any(|e| e.id == id && access.reaches(agent, e));
                let reviewable = reaches
                    && (kh.pinnable(agent, &cfg, id).await?
                        || (action == "remove"
                            && kh
                                .visible(agent, &cfg, &identity, &access, false)
                                .await?
                                .iter()
                                .any(|e| e.id == id)));
                if !reviewable {
                    return Ok(refused());
                }
                let done = if action == "pin" {
                    kh.pin(agent, &cfg, id).await?
                } else {
                    kh.retire(agent, id).await?
                };
                Ok(if done {
                    ToolResult::success(serde_json::json!({ "id": id, "done": action }), ms())
                } else {
                    refused()
                })
            }
            other => Ok(ToolResult::failure(
                format!("Unknown action `{other}`"),
                ms(),
            )),
        }
    }
}

impl KnowHowTool {
    async fn add(
        &self,
        input: Input,
        identity: &str,
        sites: &Sites,
        ms: impl Fn() -> u64,
    ) -> Result<ToolResult> {
        let kh = &self.know_how;
        let agent = self.agent.as_str();
        let (Some(db), Some(tables)) = (input.database_id, input.tables) else {
            return Ok(ToolResult::failure(
                "add needs database_id and tables",
                ms(),
            ));
        };
        let Some(site) = sites.site_of(input.instance.as_deref()).map(str::to_string) else {
            return Ok(ToolResult::failure(
                "No Metabase instance you can reach matches; pass `instance`.",
                ms(),
            ));
        };
        let Some(schema) = kh.schema_of(&site, db).await else {
            return Ok(ToolResult::failure(
                format!("The schema of database {db} is not known yet; run a query on it first."),
                ms(),
            ));
        };
        let mut canon = Vec::new();
        for t in &tables {
            let probe = super::sql::analyse(&format!("SELECT * FROM {t}"), &schema);
            match probe.tables.first() {
                Some(c) => canon.push(c.clone()),
                None => {
                    return Ok(ToolResult::failure(
                        format!("Database {db} has no table `{t}`."),
                        ms(),
                    ))
                }
            }
        }
        canon.sort();
        canon.dedup();
        // `table.column`, the table being the first named that has it.
        let qualified = |c: &str| {
            canon.iter().find_map(|t| {
                schema
                    .columns
                    .get(t)?
                    .iter()
                    .find(|k| k.eq_ignore_ascii_case(c))
                    .map(|k| format!("{t}.{k}"))
            })
        };
        let mut columns: Vec<String> = Vec::new();
        for c in input.columns.unwrap_or_default() {
            match qualified(&c) {
                Some(k) => columns.push(k),
                None => {
                    return Ok(ToolResult::failure(
                        format!("No column `{c}` in {}.", canon.join(", ")),
                        ms(),
                    ))
                }
            }
        }
        columns.sort();
        columns.dedup();
        let Some(person) = kh.person(agent, identity) else {
            return Ok(ToolResult::failure(
                "Know-how cannot record on this install (no vault key).",
                ms(),
            ));
        };
        let admin = (self.is_admin)();
        let intent = match input.about.as_deref() {
            Some(about) => {
                let Some(text) = screen_intent(about) else {
                    return Ok(ToolResult::failure(
                        "`about` must be a short generic description with no names, numbers or dates.",
                        ms(),
                    ));
                };
                kh.embed_intent(&text).await
            }
            None => None,
        };
        let note = if admin {
            input.note.map(|n| n.chars().take(200).collect())
        } else {
            None
        };
        let key = format!("manual:{}", location_key(&site, db, &canon, &columns));
        let id = kh
            .store()
            .record(
                agent,
                Observation {
                    site,
                    database_id: db,
                    tables: canon.clone(),
                    columns,
                    note,
                    intent,
                    manual: true,
                    pin: admin,
                    ..Observation::of(Kind::DataLocation, key, person)
                },
            )
            .await?;
        kh.forget_snapshot(agent);
        let status = if admin {
            "in use for everyone now"
        } else {
            "in use for you; for everyone once an admin pins it"
        };
        Ok(ToolResult::success(
            serde_json::json!({ "id": id, "tables": canon, "status": status }),
            ms(),
        ))
    }
}

impl KnowHow {
    /// Embed a generic description someone gave, for matching.
    pub async fn embed_intent(&self, text: &str) -> Option<super::IntentVector> {
        let embedder = self.embedder.as_ref()?;
        let mut v = embedder
            .embed_many(
                &[text.to_string()],
                workforce_memory::embedding::EmbedKind::Query,
            )
            .await
            .ok()?;
        (!v.is_empty()).then(|| super::IntentVector {
            model: embedder.model_id().to_string(),
            vector: v.remove(0),
        })
    }
}
