//! What the pipelines show the hub: the reads it serves.

use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::exec_runner::protocol::PROTOCOL_VERSION;
use workforce_orchestrator::workflow::instance::{booked_ticket, StepRecord};

use crate::hub_core::grants::GrantStatus;
use crate::hub_core::schedules::Outcomes;
use chrono::DateTime;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use workforce_common::persistence::InstanceFilter;
use workforce_common::persistence::WorkflowInstanceRow;

/// What the hub knows beyond a pipeline's documents: the workflow runs
/// `pipeline.runs` shows and the facts `pipeline.facts` composes.
#[async_trait::async_trait]
pub trait HubReads: Send + Sync {
    /// The runs `filter` admits that execute as `agent` (any agent when
    /// `None`), most recently updated first, each with the agent it executes
    /// as.
    async fn runs(&self, filter: &InstanceFilter, agent: Option<&str>) -> eyre::Result<Vec<Run>>;
    /// What the workflows serving a pipeline depend on: an agent's workflows
    /// on its pipeline, else the workflows `sources` names. Only the skills
    /// `caller` may run are named, with tools grants read for its identity.
    async fn facts(
        &self,
        agent: Option<&str>,
        sources: &[String],
        caller: &Caller,
    ) -> eyre::Result<Facts>;
}

/// A workflow instance as the item views read it.
#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub workflow_id: String,
    /// The agent the run executes as; see [`runs_as_declared`].
    pub agent_id: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// The step it is on, or stopped at.
    pub current_step: Option<String>,
    /// When the hub process executing it loses it, unless it renews.
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Why it failed, as the run recorded it.
    pub failure_reason: Option<String>,
    /// The ticket the run books its cost to, when it names one.
    pub booked: Option<String>,
    pub steps: Vec<StepRecord>,
}

impl From<WorkflowInstanceRow> for Run {
    fn from(row: WorkflowInstanceRow) -> Self {
        let steps: Vec<StepRecord> = serde_json::from_str(&row.step_history).unwrap_or_else(|e| {
            tracing::warn!(run = %row.id, error = %e, "step history does not parse; reading the run without steps");
            Vec::new()
        });
        let variables: Value = serde_json::from_str(&row.variables).unwrap_or(Value::Null);
        Self {
            booked: booked_ticket(&variables).map(str::to_string),
            failure_reason: text(&variables, "_failure_reason").map(str::to_string),
            lease_expires_at: row
                .lease_expires_at
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                .map(|at| at.with_timezone(&Utc)),
            current_step: row.current_step_id,
            steps,
            id: row.id,
            workflow_id: row.workflow_id,
            agent_id: row.agent_id,
            status: row.status,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

pub fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[derive(Debug, Clone, Serialize)]
pub struct Facts {
    /// The running build: version, commit, image tag and build date.
    pub build: Value,
    pub workers: Vec<WorkerFacts>,
    pub skills: Vec<SkillFacts>,
    pub workflows: Vec<WorkflowFacts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillFacts {
    pub name: String,
    pub version: String,
    /// The commit a GitHub install pinned; `None` for any other install.
    pub source_commit: Option<String>,
    pub sha256: String,
    pub contract_fingerprint: String,
    pub tools: Vec<String>,
    /// `None` when the skill declares no platform tools to grant.
    pub grant: Option<Grant>,
}

/// A skill's tools grant: the reading identity's own, and the org-wide one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// `None` when the facts are read without an identity.
    pub run_as: Option<GrantStatus>,
    pub shared: GrantStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowFacts {
    /// The key a run addresses it by: its id, owner-prefixed for a member's copy.
    pub id: String,
    pub version: String,
    /// BLAKE3 of the stored definition; `None` for one seeded from disk.
    pub content_hash: Option<String>,
    /// The tools its tool steps call.
    pub tools: Vec<String>,
    /// The skills a run needs, each with the semver requirement its
    /// installed version must meet.
    pub requires: BTreeMap<String, String>,
    pub schedules: Vec<ScheduleFacts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScheduleFacts {
    pub job_id: String,
    pub cron: Option<String>,
    pub tz: Option<String>,
    pub enabled: bool,
    pub next_run: Option<DateTime<Utc>>,
    pub last_run: Option<DateTime<Utc>>,
    /// The latest [`LAST_RUNS`] runs this hub holds, classified.
    pub last_12: Outcomes,
}

/// What this hub last heard from a runner daemon: its health and the
/// protocol it announced. A worker is named by the kind of endpoint it is,
/// never by its address or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkerFacts {
    /// `remote` for the `[exec.remote]` worker, `local` for the hub's own runner.
    pub endpoint: &'static str,
    /// The protocol it last announced, 0 when it announced none; `None`
    /// until a connection hears from it.
    pub protocol_version: Option<u32>,
    /// The protocol this hub speaks.
    pub required_protocol: u32,
    pub version: Option<String>,
    pub capacity: Option<u32>,
    pub last_health_at: Option<DateTime<Utc>>,
    /// When a connection last heard from it.
    pub last_seen_at: Option<DateTime<Utc>>,
    pub last_refusal: Option<ProtocolRefusal>,
}

/// A request refused because the daemon's protocol was older than it needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ProtocolRefusal {
    pub at: DateTime<Utc>,
    pub announced: Option<u32>,
    pub required: u32,
}

impl WorkerFacts {
    pub fn unheard(endpoint: &'static str) -> Self {
        Self {
            endpoint,
            protocol_version: None,
            required_protocol: PROTOCOL_VERSION,
            version: None,
            capacity: None,
            last_health_at: None,
            last_seen_at: None,
            last_refusal: None,
        }
    }
}

/// Who a document belongs to. Mirrors the vault's tiers so a reader who
/// knows one knows the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum Scope {
    /// Private to one identity.
    User(String),
    /// Shared by every run of one agent.
    Agent(String),
    /// Readable by everyone; written by admins and system runs.
    Global,
}

impl Scope {
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::User(_) => "user",
            Scope::Agent(_) => "agent",
            Scope::Global => "global",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Scope::User(id) | Scope::Agent(id) => id,
            Scope::Global => "",
        }
    }

    /// Rebuild from the two stored columns.
    pub fn from_parts(kind: &str, id: &str) -> Option<Scope> {
        match kind {
            "user" => Some(Scope::User(id.to_string())),
            "agent" => Some(Scope::Agent(id.to_string())),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }

    /// Parse the spelling tools and routes accept: `user` (the caller's
    /// own), `user:<id>`, `agent` (the calling agent's own), `agent:<id>`,
    /// `global`. The bare forms resolve against `caller`.
    pub fn parse(spec: &str, caller: &Caller) -> Result<Scope, StoreError> {
        let spec = spec.trim();
        let (kind, id) = match spec.split_once(':') {
            Some((k, id)) => (k.trim(), Some(id.trim())),
            None => (spec, None),
        };
        match (kind, id) {
            ("global", None) => Ok(Scope::Global),
            ("global", Some(_)) => Err(StoreError::Invalid("scope `global` takes no id".into())),
            ("user", None) => caller
                .identity
                .clone()
                .map(Scope::User)
                .ok_or(StoreError::NoIdentity),
            ("user", Some(id)) if !id.is_empty() => Ok(Scope::User(id.to_string())),
            ("agent", None) => caller
                .agent_id
                .clone()
                .map(Scope::Agent)
                .ok_or_else(|| StoreError::Invalid("no calling agent to scope to".into())),
            ("agent", Some(id)) if !id.is_empty() => Ok(Scope::Agent(id.to_string())),
            _ => Err(StoreError::Invalid(format!(
                "unknown scope `{spec}` — expected user, user:<id>, agent, agent:<id> or global"
            ))),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            other => write!(f, "{}:{}", other.kind(), other.id()),
        }
    }
}

/// Who is asking. Built by each surface from what it knows: the task-local
/// identity for an agent turn, the gateway principal for a route, the
/// skill's execution context for a WASM host call.
#[derive(Debug, Clone, Default)]
pub struct Caller {
    /// The human identity, when one is set. A `user` scope needs it.
    pub identity: Option<String>,
    /// The agent whose turn or workflow this is.
    pub agent_id: Option<String>,
    /// Operator / admin: may write `global` and reach any `agent` scope.
    pub admin: bool,
    /// A system run (no human behind it) — may write `global` and read any
    /// `user` scope, the way the vault treats the `system` identity.
    pub system: bool,
}

impl Caller {
    /// Whether this caller may perform `access` on `scope`. The rules are
    /// the vault's: your own `user` scope only; your own agent's scope, or
    /// any with admin; `global` reads for everyone, writes for admins and
    /// system runs.
    pub fn authorise(&self, scope: &Scope, access: Access) -> Result<(), StoreError> {
        let allowed = match scope {
            Scope::User(id) => self.system || self.identity.as_deref() == Some(id.as_str()),
            Scope::Agent(id) => {
                self.admin || self.system || self.agent_id.as_deref() == Some(id.as_str())
            }
            Scope::Global => match access {
                Access::Read => true,
                Access::Write => self.admin || self.system,
            },
        };
        if allowed {
            Ok(())
        } else {
            Err(StoreError::Denied(match (scope, access) {
                (Scope::User(_), _) => "a user scope is private to that identity".into(),
                (Scope::Agent(id), _) => {
                    format!("agent scope `{id}` belongs to another agent (admins may reach it)")
                }
                (Scope::Global, _) => "only admins and system runs write the global scope".into(),
            }))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("not found")]
    NotFound,
    #[error("version conflict: expected {expected}, document is at {actual}")]
    VersionConflict { expected: i64, actual: i64 },
    #[error("claimed by {owner} until {until}")]
    Claimed { owner: String, until: DateTime<Utc> },
    #[error("{0}")]
    Denied(String),
    #[error("a user scope needs an identity, and this run has none")]
    NoIdentity,
    #[error("{0}")]
    Invalid(String),
    #[error("store backend: {0}")]
    Backend(String),
    /// Something this backend does not do, rather than something it refuses.
    /// Full-text search is the only one: it is Postgres text-search semantics,
    /// which the in-memory backend cannot reproduce and will not approximate.
    #[error("{0}")]
    Unsupported(String),
}

impl StoreError {
    /// A short machine-readable tag for tool results and HTTP bodies.
    pub fn code(&self) -> &'static str {
        match self {
            StoreError::NotFound => "not_found",
            StoreError::VersionConflict { .. } => "version_conflict",
            StoreError::Claimed { .. } => "claimed",
            StoreError::Denied(_) => "denied",
            StoreError::NoIdentity => "no_identity",
            StoreError::Invalid(_) => "invalid",
            StoreError::Backend(_) => "backend",
            StoreError::Unsupported(_) => "unsupported",
        }
    }
}

impl From<eyre::Report> for StoreError {
    fn from(e: eyre::Report) -> Self {
        StoreError::Backend(format!("{e:#}"))
    }
}
