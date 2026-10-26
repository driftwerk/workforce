use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::postgres::PostgresConfig;
use crate::session::{CompactionConfig, SessionContextType, SpillConfig};
use crate::subscription::{CredentialTier, SharedServes};

/// Claim grounding — fact-checks a code-shaped answer's claims against the
/// project source and flags any the source contradicts, so the human knows
/// what to double-check. Off by default; flip to `true` to enable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimGroundingConfig {
    pub enabled: bool,
    /// Append a deterministic notice listing tool families that failed with
    /// no later success, so the reader knows which claims may rest on actions
    /// that never happened. No LLM involved. Off by default: the agent sees
    /// its own tool errors and says so, and on installs with flaky adapters
    /// the notice stamped most replies. Read directly, independent of
    /// `enabled`.
    #[serde(default)]
    pub flag_failed_tools: bool,
    /// Ground the answer's data claims (numbers/counts/names/dates attributed
    /// to tool-fetched data) against this turn's own tool outputs and staged
    /// artifacts. Independent of `enabled` (which gates the code lens) — read
    /// directly per turn, for per-agent rollout. Off by default.
    #[serde(default)]
    pub data_claims: bool,
    /// Post-turn commitment check: one classifier call on the final text; an
    /// unconditional future commitment with no successful
    /// scheduler/reminder/jobs/watch/monitor/heartbeat call this turn appends
    /// a visible caveat. Off by default.
    #[serde(default)]
    pub commitments: bool,
    /// Model for the grounding passes (claim verification, conclusion review,
    /// the editor's code/domain classifier). Accepts a concrete model id or a
    /// tier shorthand (e.g. "cheap", "provider:tier") resolved like the other
    /// background-task model fields. `None` = the agent's default model.
    #[serde(default)]
    pub model: Option<String>,
    /// Wall-clock budget for a review that runs on the REPLY PATH — gathering
    /// plus write-up, from the answer being complete on screen to the turn
    /// settling. `None` = [`DEFAULT_REVIEW_TIMEOUT_SECS`].
    ///
    /// Tunable because it is a straight trade of the reader's time against how
    /// much of the answer gets checked, and the right point differs per install:
    /// a repo the research tools must grep hard costs more rounds to reach the
    /// same coverage than a small one.
    #[serde(default)]
    pub review_timeout_secs: Option<u64>,
    /// Of [`Self::review_timeout_secs`], the slice held back for the review to
    /// say what it found. `None` = [`DEFAULT_REVIEW_SYNTHESIS_RESERVE_SECS`].
    #[serde(default)]
    pub review_synthesis_reserve_secs: Option<u64>,
}

/// Default wall-clock budget for a review on the REPLY PATH — between the answer
/// being complete on screen and the turn settling.
///
/// The history matters, because the obvious "fix" for a slow reply is to cut
/// this back and that would restore a check which verifies nothing:
///
/// - **180s**, shared with the background bound, was a ceiling rather than a
///   budget. Measured on harbor, the median pass cost the reader 65s of
///   `… still writing` on an answer they had already finished reading.
/// - **40s** removed the dead air and with it the product. Over 23h, 43% of
///   passes ended on the research deadline with an EMPTY write-up, and the total
///   number of claims checked across the whole day was 8. The advisor loop costs
///   its *next* round at the slowest round seen so far, so against a 25s
///   research window a single 10s `repo.grep` ends gathering immediately.
/// - **90s** is affordable now for a reason that did not hold in either era:
///   `ProgressEvent::AnswerSettled` settles the answer on screen *before* the
///   reviews start, and the reviews run concurrently. This budget buys
///   annotation time on an answer the reader already has — it is not dead air,
///   which is what the 65s complaint was actually about.
///
/// A budget that never completes is not cheap; it is the whole price for none of
/// the product.
pub const DEFAULT_REVIEW_TIMEOUT_SECS: u64 = 90;

/// Default slice of the budget reserved for the write-up.
///
/// A review is two phases with very different costs: cheap tool rounds that
/// gather evidence, then one call carrying the question, the answer and every
/// tool result, which writes the verdict. Gathering stops at
/// `timeout - reserve` and the rest is the verdict's, so a review that runs long
/// narrows instead of vanishing.
pub const DEFAULT_REVIEW_SYNTHESIS_RESERVE_SECS: u64 = 20;

impl ClaimGroundingConfig {
    /// The reply-path review budget as `(total, write-up reserve)`, validated.
    ///
    /// A reserve of zero is the discard-everything behaviour the split exists to
    /// remove, and a reserve at or above the whole budget leaves nothing to
    /// gather — so a configuration asking for either is corrected here rather
    /// than obeyed. Callers get a pair that always satisfies
    /// `0 < reserve < total`.
    pub fn review_budget(&self) -> (Duration, Duration) {
        let total = self
            .review_timeout_secs
            .unwrap_or(DEFAULT_REVIEW_TIMEOUT_SECS)
            .max(2);
        let reserve = self
            .review_synthesis_reserve_secs
            .unwrap_or(DEFAULT_REVIEW_SYNTHESIS_RESERVE_SECS)
            .clamp(1, total - 1);
        (Duration::from_secs(total), Duration::from_secs(reserve))
    }
}

/// Channel-monitor engine settings (`[monitor]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Consecutive failed sweeps of one channel before every enabled monitor
    /// bound to it is auto-disabled (with an owner DM naming the error and the
    /// re-enable command). `0` turns auto-disable off.
    #[serde(default = "default_auto_disable_after_failures")]
    pub auto_disable_after_failures: u32,
    /// How far back a sweep may reach for a monitor that has been out of
    /// contact — its bot uninvited, or an edit awaiting its owner. `0` removes
    /// the bound. Clamped to the dispatch retention, so a message the sweep
    /// re-offers always still has the dispatch row that suppresses it.
    #[serde(default = "default_catch_up_window_hours")]
    pub catch_up_window_hours: u64,
    /// The shortest `every_secs` a source monitor may ask for.
    #[serde(default = "default_min_poll_secs")]
    pub min_poll_secs: u64,
    /// How often the engine looks for source monitors whose poll is due.
    #[serde(default = "default_poll_tick_secs")]
    pub poll_tick_secs: u64,
    /// The most items one poll of a source observes; the rest are cut.
    #[serde(default = "default_max_items")]
    pub max_items: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            auto_disable_after_failures: default_auto_disable_after_failures(),
            catch_up_window_hours: default_catch_up_window_hours(),
            min_poll_secs: default_min_poll_secs(),
            poll_tick_secs: default_poll_tick_secs(),
            max_items: default_max_items(),
        }
    }
}

fn default_auto_disable_after_failures() -> u32 {
    30
}

fn default_catch_up_window_hours() -> u64 {
    24
}

fn default_min_poll_secs() -> u64 {
    60
}

fn default_poll_tick_secs() -> u64 {
    15
}

fn default_max_items() -> usize {
    500
}

/// Answer-ready code context — before the model's first turn, assemble the
/// complete relevant source for a code question and inject it so the model reads
/// real current code instead of guessing. OFF by default: enable it per-install
/// (`enabled = true`) once validated on that install's repos. It is the core fix
/// for the code-answer path, but rolled out per-install rather than flipped on
/// globally for untested repos/languages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeContextConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Minimum specificity-weighted score the primary repo must reach to inject
    /// (the corroboration path). Higher = inject less often.
    #[serde(default = "default_min_grounding_score")]
    pub min_grounding_score: f32,
    /// A single identifier-shaped strong match (strength × shape, IDF-independent)
    /// that alone admits injection — the specific-symbol path.
    #[serde(default = "default_strong_specific_score")]
    pub strong_specific_score: f32,
    /// Single-repo fallback: with one repo indexed, admit on a plain name-match at
    /// this strength (IDF is uninformative there).
    #[serde(default = "default_min_name_match")]
    pub min_name_match: f32,
    /// A secondary repo contributes only if its score is at least this fraction of
    /// the primary's.
    #[serde(default = "default_secondary_repo_margin")]
    pub secondary_repo_margin: f32,
    /// Weight of a bare lowercase English-word pattern relative to an identifier's
    /// (1.0), so a generic word colliding with a symbol cannot alone inject.
    #[serde(default = "default_bare_word_shape_weight")]
    pub bare_word_shape_weight: f32,
    /// Weight on the primary repo's best semantic score, a bounded corroborator.
    #[serde(default = "default_vector_bonus_weight")]
    pub vector_bonus_weight: f32,
    /// Gate injection on question intent: assemble code context only when the
    /// question actually seeks code (a code-intent phrase or a code-symbol
    /// reference), so data/ops/chat questions that merely collide with symbol
    /// names get none. `true` (default) = gate on; `false` = legacy behaviour
    /// (attempt assembly for every question, relevance self-gate only).
    #[serde(default = "default_require_code_intent")]
    pub require_code_intent: bool,
}

impl Default for CodeContextConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_grounding_score: default_min_grounding_score(),
            strong_specific_score: default_strong_specific_score(),
            min_name_match: default_min_name_match(),
            secondary_repo_margin: default_secondary_repo_margin(),
            bare_word_shape_weight: default_bare_word_shape_weight(),
            vector_bonus_weight: default_vector_bonus_weight(),
            require_code_intent: default_require_code_intent(),
        }
    }
}

fn default_min_grounding_score() -> f32 {
    4.0
}
fn default_strong_specific_score() -> f32 {
    3.5
}
fn default_min_name_match() -> f32 {
    3.0
}
fn default_secondary_repo_margin() -> f32 {
    0.75
}
fn default_bare_word_shape_weight() -> f32 {
    0.35
}
fn default_vector_bonus_weight() -> f32 {
    1.5
}
fn default_require_code_intent() -> bool {
    true
}

/// Root configuration for the workforce system.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkforceConfig {
    /// Hub configuration.
    #[serde(default)]
    pub hub: HubConfig,
    /// Claim grounding.
    #[serde(default)]
    pub claim_grounding: ClaimGroundingConfig,
    /// Privacy gateway routing for Claude Code subprocesses (`[privacy]`).
    /// OFF by default; install-wide and read at boot.
    #[serde(default)]
    pub privacy: PrivacyConfig,
    /// System One decision backends (`[decision]`): Kev, Jev, or both.
    /// OFF by default; install-wide and read at boot.
    #[serde(default)]
    pub decision: DecisionConfig,
    /// What scheduled jobs may do (`[scheduler]`); each agent's own
    /// `config.toml` can narrow it.
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    /// Subscription routes install-wide (`[subscription]`). Read at boot.
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    /// How tool calls and tool definitions are handled on their way to and
    /// from the model (`[tools]`). Install-wide and read at boot.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Channel-monitor engine settings.
    #[serde(default)]
    pub monitor: MonitorConfig,
    /// Answer-ready code-context injection (`[code_context]`). ON by default;
    /// resolved per-agent (workspace override → global) and hot-reloaded.
    #[serde(default)]
    pub code_context: CodeContextConfig,
    /// NATS connection settings.
    #[serde(default)]
    pub nats: NatsConfig,
    /// Multi-instance expectations (`[cluster]`).
    #[serde(default)]
    pub cluster: ClusterConfig,
    /// Weaviate connection settings (legacy — prefer [memory] section).
    #[serde(default)]
    pub weaviate: WeaviateConfig,
    /// Memory store configuration.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// vellev connection settings, used when memories, the artifact index
    /// or code vectors are kept there.
    #[serde(default)]
    pub vellev: VellevConfig,
    /// Where memory vectors come from. Unset (the default), Weaviate's own
    /// vectorizer embeds memories as before; set, the hub embeds them and the
    /// memory store moves to a collection that holds the hub's vectors.
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    /// Shared know-how an agent learns from how it is used (where data lives,
    /// query shapes that worked), shown to its users when a question matches.
    /// Off unless enabled here and, under `opt_in`, in the agent's own config.
    #[serde(default)]
    pub know_how: KnowHowConfig,
    /// How `investigate` holds findings to account; the install keeps only the
    /// adversarial kill switch, each agent's `config.toml` the rest.
    #[serde(default)]
    pub research: ResearchConfig,
    /// How work handed between steps and sub-agents is verified; the install
    /// keeps only the kill switch, each agent's `config.toml` the rest.
    #[serde(default)]
    pub handoff: HandoffConfig,
    /// How attachments are turned into model input (`[attachments]`).
    #[serde(default)]
    pub attachments: AttachmentsConfig,
    /// LLM provider settings.
    #[serde(default)]
    pub llm: LlmConfig,
    /// Agent definitions.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Team definitions.
    #[serde(default)]
    pub teams: Vec<TeamConfig>,
    /// Budget configuration.
    #[serde(default)]
    pub budget: BudgetConfig,
    /// Gateway (WebSocket + REST) configuration.
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// Skills system configuration.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Secrets vault configuration.
    #[serde(default)]
    pub vault: VaultConfig,
    /// Optional Postgres backend connection settings. When `enabled = true`,
    /// individual stores can opt into a Postgres backend in their own
    /// config (mirrors the existing `MemoryConfig.backend` pattern).
    /// Default: disabled — every store uses its existing SQLite/file
    /// backend. See `docs/POSTGRES.md` for the migration roadmap.
    #[serde(default)]
    pub postgres: Option<PostgresConfig>,
    /// Telemetry / cost / audit store configuration.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Conversation session store configuration. Selects which backend
    /// (JSONL files default, or the shared Postgres backend) holds the
    /// live transcript + archive. Not enabled implicitly by `[postgres]`
    /// — sessions must be backfilled before cutover, so it is explicit.
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// Scheduler job store configuration. Selects which backend (per-agent
    /// `jobs.json` default, or shared Postgres rows) persists scheduled
    /// jobs. Explicit opt-in like sessions: cutover needs a
    /// `migrate-scheduler` backfill first.
    #[serde(default)]
    pub scheduler_store: SchedulerStoreConfig,
    /// Runtime artifact store configuration. Selects which backend
    /// (local filesystem default, or shared S3) holds large tool outputs
    /// so any hub instance can serve an artifact another produced. Not a
    /// Postgres store; the S3 backend fails closed at boot.
    #[serde(default)]
    pub artifacts: ArtifactsConfig,
    /// Revision-history retention (workflows, playbooks, identity files,
    /// config overlay).
    #[serde(default)]
    pub revisions: RevisionsConfig,
    /// Identity / team store configuration. Selects which backend
    /// (SQLite default, or optional Postgres) holds users + teams.
    #[serde(default)]
    pub identity: IdentityConfig,
    /// Permission store + cascade evaluator configuration. Disabled by
    /// default while phases 1–6 of the RBAC + permission cascade are
    /// being rolled out. Set `enabled = true` to surface the admin UI
    /// and the cascade-driven runtime decisions.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// Approval-routing (designated-approvers) configuration. Disabled
    /// by default while the staged rollout is in progress — set
    /// `enabled = true` to route `RequireApproval` decisions to named
    /// approvers via Slack DMs with a durable pending-approval store.
    #[serde(default)]
    pub approvals: ApprovalsConfig,
    /// Environments configuration. Disabled by default; when `enabled = true`,
    /// a named environment pins every multi-instance skill (and MCP workspace)
    /// to one coherent target (e.g. all-prod) so the agent cannot muddle
    /// environments in a single answer.
    #[serde(default)]
    pub environments: EnvironmentsConfig,
    /// Pairing store configuration. Selects which backend
    /// (SQLite default, or optional Postgres) holds pairing requests,
    /// approved devices, and the channel allowlist. Distinct from the
    /// existing `[pairing]` runtime-policy block.
    #[serde(default, rename = "pairing_store")]
    pub pairing_store: PairingStoreConfig,
    /// Project store configuration. Selects which backend
    /// (SQLite default, or optional Postgres) holds per-agent project
    /// manifests (named multi-repo bundles for /cc + the knowledge
    /// indexer + non-CC agent worktrees).
    #[serde(default)]
    pub projects: ProjectsConfig,
    /// Code knowledge graph store configuration. Selects which
    /// backend holds the entity / relationship / file_index metadata
    /// for `workforce-codebase-knowledge`. Vector embeddings remain
    /// on Weaviate regardless.
    #[serde(default)]
    pub codegraph: CodegraphConfig,
    /// GitHub integration configuration.
    #[serde(default)]
    pub github: GithubConfig,
    /// Legacy Slack integration configuration (deprecated — use per-skill config).
    /// Kept for TOML parse compatibility but no longer used at runtime.
    #[serde(default)]
    slack: Option<toml::Value>,
    /// Max characters from daily notes to include in the system prompt.
    /// Today gets `max_daily_notes_chars`, yesterday gets half.
    /// Default: 8000 (~2000 tokens).
    #[serde(default = "default_max_daily_notes_chars")]
    pub max_daily_notes_chars: usize,
    /// Compaction / context window management configuration.
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Auto-spill of large tabular tool outputs.
    #[serde(default)]
    pub spill: SpillConfig,
    /// Session lifecycle management.
    #[serde(default)]
    pub session_lifecycle: SessionLifecycleConfig,
    /// Per-type and per-channel session reset policies.
    #[serde(default)]
    pub session_policies: SessionPoliciesConfig,
    /// Heartbeat configuration.
    #[serde(default)]
    pub heartbeat: HeartbeatSettings,
    /// Shared channel coordination (multi-agent channels).
    #[serde(default)]
    pub shared_channels: Vec<SharedChannelEntry>,
    /// Per-group configuration overrides keyed by channel/group ID.
    /// Allows customising response mode, system prompt, keywords, etc.
    #[serde(default)]
    pub group_overrides: HashMap<String, GroupOverride>,
    /// Group chat behaviour configuration.
    #[serde(default)]
    pub groups: GroupsConfig,
    /// User timezone (IANA name, e.g. "Europe/London", "America/New_York").
    /// Defaults to UTC if not set or invalid.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Pairing configuration (channel access control, device/API pairing, node pairing).
    #[serde(default)]
    pub pairing: workforce_pairing::PairingConfig,
    /// Sentinel security configuration (trust classification, owner IDs).
    #[serde(default)]
    pub sentinel: crate::sentinel_config::SentinelConfig,
    /// DM routing: per-user agent assignment for direct messages (DW-148).
    #[serde(default)]
    pub dm_routing: DmRoutingConfig,
    /// Daily briefing configuration (DW-168).
    #[serde(default)]
    pub briefing: BriefingConfig,
    /// Agent communications oversight configuration (DW-219).
    #[serde(default)]
    pub agent_comms: AgentCommsSettings,
    /// Codebase knowledge engine configuration.
    #[serde(default)]
    pub knowledge: KnowledgeConfig,
    /// MCP (Model Context Protocol) server configuration.
    #[serde(default)]
    pub mcp: McpConfig,
    /// MCP server install persistence (separate from `[mcp]` runtime
    /// config — this picks the backend that stores agent-driven and
    /// UI-driven MCP server installs across hub restarts).
    #[serde(default, rename = "mcp_store")]
    pub mcp_store: McpStoreConfig,
    /// Remote MCP: the hub serving MCP at `/mcp` for external clients acting
    /// as a workforce user (`[remote_mcp]`). Distinct from `[mcp]`, which is
    /// the hub as a client plus the session MCP for hub-spawned Claude Code.
    #[serde(default)]
    pub remote_mcp: RemoteMcpConfig,
    /// OAuth pending-redirect state store. Default in-memory works
    /// for restart-stable flows; sqlite persists across restarts so a
    /// UI-initiated MCP install can complete its OAuth callback even
    /// if the hub bounced between authorise-click and vendor-callback.
    #[serde(default, rename = "oauth_state")]
    pub oauth_state: OAuthStateStoreConfig,
    /// `/cc` Claude Code chat configuration.
    #[serde(default)]
    pub claude_chat: ClaudeChatConfig,
    /// `/cx` Codex chat: the same surface as `/cc` on the Codex engine.
    /// Absent `[codex_chat.engine]` follows `[coding_session.engine]` led by
    /// `codex`.
    #[serde(default)]
    pub codex_chat: ClaudeChatConfig,
    /// `coding.session` (agent-driven Claude Code) configuration.
    #[serde(default)]
    pub coding_session: CodingSessionConfig,
    /// Local agents configuration.
    #[serde(default)]
    pub local_agents: LocalAgentsConfig,
    /// Trace capture configuration for LLM fine-tuning data.
    #[serde(default)]
    pub tracing: TracingConfig,
    /// Advisor configuration (second-opinion / review). Default: disabled.
    #[serde(default)]
    pub advisor: AdvisorConfig,
    /// Doc-curator configuration (`workforce docs curate`).
    /// Install-level default; CLI `--tier` overrides per invocation.
    #[serde(default)]
    pub curator: CuratorConfig,
    /// Per-agent training corpora (DW-536).
    #[serde(default)]
    pub training: TrainingConfig,
    /// Workflow/playbook execution limits.
    #[serde(default)]
    pub workflows: WorkflowsConfig,
    /// AWS S3 read-only tool (`aws.s3.{list,head,get}`). Disabled unless
    /// `enabled = true` AND `bucket_allowlist` is non-empty AND
    /// `assume_role_arn` is set; with any unset the tool isn't registered.
    #[serde(default, rename = "aws_s3")]
    pub aws_s3: AwsS3Config,
    /// AWS read-only infra-diagnosis tools (`aws.ecs/ecr/elbv2/rds/sts.*`,
    /// `aws.deploy.status`). Disabled unless `enabled = true` AND
    /// `assume_role_arn` is set.
    #[serde(default, rename = "aws_infra")]
    pub aws_infra: AwsInfraConfig,
    /// Let read-only worker sub-agents acquire additional read-only tool
    /// surfaces (AWS/Slack/GitHub/Notion/Google) on demand via `read_source.*`.
    /// Off by default: it widens what an autonomous worker can read, so an
    /// operator opts in.
    #[serde(default, rename = "worker_read_sources")]
    pub worker_read_sources: WorkerReadSourcesConfig,
    /// Agent-curated self-learning (`[self_learning]`). A scheduled, read-only
    /// curation loop producing primary-cited knowledge items. Off by default;
    /// per-agent hot-reload override mirrors `[worker_read_sources]`.
    #[serde(default, rename = "self_learning")]
    pub self_learning: SelfLearningConfig,
    /// Team-building games site (`/games`). Disabled by default; when enabled
    /// the hub mounts the games router and seats the listed game-player agents.
    #[serde(default)]
    pub games: GamesConfig,
    /// Privilege-separated command executor (`exec.run` / `code.*`). When a
    /// runner socket is set the hub dispatches commands to the unprivileged
    /// runner daemon instead of executing them in-process (audit F7).
    #[serde(default)]
    pub exec: ExecConfig,
}

/// `[exec]` — privilege-separated command execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecConfig {
    /// Unix socket of the runner daemon. When set, every execution-class tool
    /// dispatches to the daemon (which runs as the unprivileged `agent` user)
    /// instead of executing in-process; unset = in-process (dev default). A
    /// configured-but-unreachable socket is fail-closed — the tool errors
    /// rather than falling back to in-process execution, which would re-expose
    /// the vault the executor exists to protect.
    #[serde(default)]
    pub runner_socket: Option<PathBuf>,
    /// Dispatch Claude Code (`coding.session`, `coding.claude`, `/cc`) to the
    /// runner as well, so it runs as the unprivileged user like every other
    /// execution-class tool.
    ///
    /// Separate from `runner_socket` on purpose: enabling the runner must not
    /// silently move Claude Code. Dispatch is fail-closed, so a mistake here
    /// takes out every coding session at once, and off is byte-for-byte the
    /// in-process path.
    #[serde(default)]
    pub sandbox_claude_code: bool,
    /// `[exec.remote]` — the worker daemon coding rounds run on, reached over
    /// mutual TLS. Independent of `runner_socket`: the local runner keeps
    /// serving `exec.run`-class commands, the worker serves scoped work. The
    /// same fail-closed rule applies to both.
    #[serde(default)]
    pub remote: Option<RemoteRunnerConfig>,
}

/// `[exec.remote]` — where the remote runner is and how both sides prove who
/// they are. The PEM fields usually arrive through `${VAR}` expansion from the
/// task's secrets.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteRunnerConfig {
    /// `host:port` of the daemon.
    #[serde(default)]
    pub addr: String,
    /// The name the daemon's certificate must carry.
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub ca_pem: String,
    #[serde(default)]
    pub cert_pem: String,
    #[serde(default)]
    pub key_pem: String,
}

impl std::fmt::Debug for RemoteRunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRunnerConfig")
            .field("addr", &self.addr)
            .field("server_name", &self.server_name)
            .field("ca_pem", &self.ca_pem.len())
            .field("cert_pem", &self.cert_pem.len())
            .field("key_pem", &"<redacted>")
            .finish()
    }
}

impl RemoteRunnerConfig {
    fn validate(&self) -> eyre::Result<()> {
        let fields = [
            ("addr", &self.addr),
            ("server_name", &self.server_name),
            ("ca_pem", &self.ca_pem),
            ("cert_pem", &self.cert_pem),
            ("key_pem", &self.key_pem),
        ];
        if let Some((key, _)) = fields.iter().find(|(_, value)| value.trim().is_empty()) {
            return Err(eyre::eyre!("[exec.remote] {key} must be set"));
        }
        if let Some((key, _)) = fields[2..]
            .iter()
            .find(|(_, value)| !value.contains("-----BEGIN "))
        {
            return Err(eyre::eyre!("[exec.remote] {key} is not PEM-encoded"));
        }
        Ok(())
    }
}

/// `[games]` — team-building games site configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GamesConfig {
    /// Mount the `/games` router and seat AI players. Default: disabled.
    #[serde(default)]
    pub enabled: bool,
    /// Agent ids seated as AI players (locked-down `*_game` profiles, e.g.
    /// `["penny_game"]`). Each should set `tool_patterns = []`, disable vault
    /// and knowledge, and point `workspace` at the real persona to keep its voice.
    #[serde(default)]
    pub agent_players: Vec<String>,
}

/// `[workflows]` — execution limits for workflows and playbooks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowsConfig {
    /// Maximum concurrent running instances per definition owner. Caps abuse
    /// from member-authored definitions; system/operator definitions are
    /// exempt. Default: 25.
    #[serde(default = "default_max_concurrent_runs_per_owner")]
    pub max_concurrent_runs_per_owner: usize,
}

impl Default for WorkflowsConfig {
    fn default() -> Self {
        Self {
            max_concurrent_runs_per_owner: default_max_concurrent_runs_per_owner(),
        }
    }
}

fn default_max_concurrent_runs_per_owner() -> usize {
    25
}

/// `[aws_s3]` — operator config for the AWS S3 read-only tool.
///
/// Access is admin-determined: `bucket_allowlist` (enforced in code as
/// defence in depth alongside the IAM `Resource` scope on the assumed
/// role) decides which buckets are readable at all. Being on the
/// allowlist is itself the authorisation decision, so `aws.s3.get`
/// defaults to `Allow` — no per-call human prompt. `require_approval_buckets`
/// is the opt-in inverse: list a sensitive bucket here to make `get` on
/// it prompt for approval (everything else flows freely). `max_get_bytes`
/// caps `get` payloads in both the head pre-check and the streaming read;
/// `0` disables the cap (note: only CSV bodies spill to the artifact
/// store — large text/JSON inline into the context window).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwsS3Config {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bucket_allowlist: Vec<String>,
    #[serde(default)]
    pub assume_role_arn: Option<String>,
    #[serde(default = "default_aws_s3_max_get_bytes")]
    pub max_get_bytes: u64,
    #[serde(default)]
    pub require_approval_buckets: Vec<String>,
}

impl Default for AwsS3Config {
    fn default() -> Self {
        Self {
            enabled: false,
            bucket_allowlist: Vec::new(),
            assume_role_arn: None,
            max_get_bytes: default_aws_s3_max_get_bytes(),
            require_approval_buckets: Vec::new(),
        }
    }
}

impl AwsS3Config {
    /// True only when every prerequisite for registration is satisfied:
    /// explicitly enabled, at least one bucket in the allowlist, and an
    /// assume-role ARN to gather temporary credentials under.
    pub fn is_ready(&self) -> bool {
        self.enabled && !self.bucket_allowlist.is_empty() && self.assume_role_arn.is_some()
    }
}

fn default_aws_s3_max_get_bytes() -> u64 {
    // 100 MiB. The 5 MiB starter cap throttled real CSV exports (tens of
    // MB); large CSV bodies spill to the artifact store rather than the
    // transcript, so a generous default is safe. Operators set `0` to
    // disable the cap entirely (large text/JSON still inline, so the cap
    // protects the context window for those kinds — see content-kind-aware
    // capping in the follow-ups).
    100 * 1024 * 1024
}

/// `[aws_infra]` — operator config for the read-only infra-diagnosis tools.
///
/// These tools return non-PII control-plane metadata (ECS/ECR/ELBv2/RDS/STS
/// describes), so the IAM role's read-only action scope is the only
/// authorisation boundary needed — there is no code-side resource allowlist,
/// unlike the S3 surface. `region` pins the SDK region (otherwise the ambient
/// `AWS_REGION` is used). The tools register only when `enabled = true` AND
/// `assume_role_arn` is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AwsInfraConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub assume_role_arn: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
}

impl AwsInfraConfig {
    /// True only when explicitly enabled with an assume-role ARN to gather
    /// temporary credentials under.
    pub fn is_ready(&self) -> bool {
        self.enabled && self.assume_role_arn.is_some()
    }
}

fn parse_csv_env(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse a boolean env var accepting the common truthy/falsy spellings used by
/// container runtimes and IaC tools. Returns `None` (with a warn) on
/// unrecognised input so the caller keeps its previous value rather than
/// silently flipping to `false` on a misconfiguration.
fn parse_bool_env(name: &str, raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        other => {
            tracing::warn!(
                env = name,
                value = %other,
                "unrecognised boolean env value; keeping previous setting",
            );
            None
        }
    }
}

/// Per-agent training corpora configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingConfig {
    #[serde(default = "default_training_enabled")]
    pub enabled: bool,
    /// Backend: `"sqlite"` (default) or `"postgres"`. The Postgres
    /// path reuses the global `[postgres]` pool; if that pool isn't
    /// configured the resolver falls back to SQLite with a warning,
    /// matching the `[mcp_store]` behaviour.
    #[serde(default = "default_training_backend")]
    pub backend: String,
    /// SQLite DB path for the training store. Relative to `[hub].data_dir`
    /// when not absolute. Only consulted when `backend = "sqlite"`.
    #[serde(default = "default_training_db_path")]
    pub db_path: String,
    /// Where original uploaded files are stored. Relative to
    /// `[hub].data_dir` when not absolute.
    #[serde(default = "default_training_data_dir")]
    pub data_dir: String,
    /// Maximum chunks pulled into the `## Agent Training` prompt block
    /// per turn.
    #[serde(default = "default_training_recall_top_k")]
    pub recall_top_k: usize,
    /// Relaxed similarity threshold for training-block recall. Below
    /// this score, a chunk is dropped. The generic recall block uses
    /// a tighter threshold (`0.4`).
    #[serde(default = "default_training_recall_threshold")]
    pub recall_threshold: f32,
    /// Minimum importance gate for the training-block recall. Training
    /// chunks are written at 0.9 importance; lower this to broaden,
    /// raise to tighten.
    #[serde(default = "default_training_min_importance")]
    pub min_importance: f32,
    /// Per-file size cap for HTTP uploads (and the future Slack ingest
    /// path). The router-level body limit caps the wire frame; this
    /// caps the parsed multipart payload, so installs can tighten
    /// without redeploying.
    #[serde(default = "default_training_max_file_size_mb")]
    pub max_file_size_mb: usize,
    /// Startup orphan-sweep settings.
    #[serde(default)]
    pub cleanup: TrainingCleanupConfig,
}

/// Startup orphan-sweep settings (DW-539 follow-up).
///
/// On hub start we cross-check `training_chunks` against the memory
/// store: SQL chunks whose Memory backing is missing get dropped, and
/// Memory rows tagged `training` without any SQL chunk get forgotten.
/// Both passes are idempotent and partial-failure tolerant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingCleanupConfig {
    #[serde(default = "default_training_cleanup_enabled")]
    pub enabled: bool,
    /// Hard timeout on the spawned sweep task. A hung memory backend
    /// won't pause hub startup beyond this.
    #[serde(default = "default_training_cleanup_timeout_secs")]
    pub timeout_secs: u64,
    /// Dry-run mode: count orphans and log but mutate nothing.
    #[serde(default)]
    pub verify_only: bool,
}

fn default_training_cleanup_enabled() -> bool {
    true
}

fn default_training_cleanup_timeout_secs() -> u64 {
    60
}

impl Default for TrainingCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: default_training_cleanup_enabled(),
            timeout_secs: default_training_cleanup_timeout_secs(),
            verify_only: false,
        }
    }
}

fn default_training_enabled() -> bool {
    true
}

fn default_training_backend() -> String {
    "sqlite".to_string()
}

fn default_training_db_path() -> String {
    "training.db".to_string()
}

fn default_training_data_dir() -> String {
    "training".to_string()
}

fn default_training_recall_top_k() -> usize {
    5
}

fn default_training_recall_threshold() -> f32 {
    0.3
}

fn default_training_min_importance() -> f32 {
    0.5
}

fn default_training_max_file_size_mb() -> usize {
    50
}

impl Default for TrainingConfig {
    fn default() -> Self {
        Self {
            enabled: default_training_enabled(),
            backend: default_training_backend(),
            db_path: default_training_db_path(),
            data_dir: default_training_data_dir(),
            recall_top_k: default_training_recall_top_k(),
            recall_threshold: default_training_recall_threshold(),
            min_importance: default_training_min_importance(),
            max_file_size_mb: default_training_max_file_size_mb(),
            cleanup: TrainingCleanupConfig::default(),
        }
    }
}

/// Backend that runs a curation turn. `Model` runs it on the agent's own
/// routes from the curation model; `ClaudeCode` runs it as a Claude Code CLI
/// turn on the run-as person's Claude login, recording through the same
/// read-only curation tools served over MCP. Per Anthropic TOS a Claude login
/// may only flow through the CLI, never the Messages API.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurationSource {
    #[default]
    Model,
    ClaudeCode,
}

/// `[self_learning]` — agent-curated knowledge: a scheduled, read-only
/// curation loop that turns named sources into primary-cited knowledge
/// items surfaced in a `## Self-Learned Knowledge` context section.
/// Off by default globally; per-agent hot-reload override mirrors
/// `[worker_read_sources]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfLearningConfig {
    /// Global gate. When `false` the runner no-ops, no store is opened,
    /// and no context section is injected.
    #[serde(default)]
    pub enabled: bool,
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_self_learning_backend")]
    pub backend: String,
    /// SQLite DB path. Relative to `[hub].data_dir` when not absolute;
    /// only consulted when `backend = "sqlite"`.
    #[serde(default = "default_self_learning_db_path")]
    pub db_path: String,
    /// Model for the curation turn (e.g. `"cheap"`). A tier name, concrete
    /// model id, or `provider:tier` routing spec, tier-resolved against the
    /// agent's provider tier like the other background-task model fields
    /// (`[memory] consolidation_model` / `passive_model`). Falls back to the
    /// agent's normal model resolution when absent — note a per-agent
    /// `[self_learning]` override replaces the whole section, so set this
    /// there too if a specific agent needs it.
    #[serde(default)]
    pub model: Option<String>,
    /// Backend for a curation turn (`"model"` or `"claude_code"`). A plan's
    /// own `source` overrides this. See [`CurationSource`].
    #[serde(default)]
    pub source: CurationSource,
    /// When Claude Code cannot serve a curation turn (no login, a login at
    /// its limit, a failed start), let the turn move on along the agent's
    /// `[llm] failover` chain, the Claude Code spec first, instead of
    /// skipping it. With no failover entries there is nothing to move on
    /// to. Off by default, mirroring `[advisor]`'s cost guard; a skipped
    /// turn is always logged so the failure is never silent.
    #[serde(default)]
    pub cc_allow_failover_to_models: bool,
    /// Max self-learned items pulled into the `## Self-Learned Knowledge`
    /// block per turn. A separate budget from Training's top-k.
    #[serde(default = "default_self_learning_recall_top_k")]
    pub recall_top_k: usize,
    /// Similarity threshold for self-learned recall — higher than
    /// Training's so it never competes with human-curated content.
    #[serde(default = "default_self_learning_recall_threshold")]
    pub recall_threshold: f32,
    /// Minimum importance gate for self-learned recall.
    #[serde(default = "default_self_learning_min_importance")]
    pub min_importance: f32,
    /// Memory importance assigned to an approved item (≤ Training's 0.9).
    #[serde(default = "default_self_learning_approved_importance")]
    pub approved_importance: f32,
    /// Memory importance assigned to a provisional item.
    #[serde(default = "default_self_learning_provisional_importance")]
    pub provisional_importance: f32,
    /// Whether non-high-stakes provisional items surface in context.
    #[serde(default = "default_true")]
    pub surface_provisional: bool,
    /// Runner cadence in seconds.
    #[serde(default = "default_self_learning_tick_secs")]
    pub tick_secs: u64,
    /// Global per-run budget guard on items recorded by one curation turn.
    #[serde(default = "default_self_learning_max_items_per_run")]
    pub max_items_per_run: u32,
    /// Default TTL (days) applied to items whose plan omits `freshness_days`.
    #[serde(default = "default_self_learning_default_freshness_days")]
    pub default_freshness_days: u32,
    /// How far back (seconds) an experiential-expertise plan mines session
    /// transcripts when it omits `transcript_lookback_secs` (default 1 week).
    #[serde(default = "default_self_learning_experiential_lookback_secs")]
    pub experiential_lookback_secs: u64,
    /// Round cap for a curation turn. Curator turns previously followed the
    /// deployment-wide `[llm] max_rounds` (50), which let deep-expertise runs
    /// grow to ~$5/run in production; 24 covers every observed productive run.
    #[serde(default = "default_self_learning_max_rounds")]
    pub max_rounds: u32,
    /// Per-run budget on NEW child nodes a deep-expertise turn may record.
    /// Bounds the tree-growth ratchet separately from `max_items_per_run`
    /// (which the atomic-items mode keeps).
    #[serde(default = "default_self_learning_deep_max_children_per_run")]
    pub deep_max_children_per_run: u32,
}

fn default_self_learning_backend() -> String {
    "sqlite".to_string()
}

fn default_self_learning_db_path() -> String {
    "self_learning.db".to_string()
}

fn default_self_learning_recall_top_k() -> usize {
    3
}

fn default_self_learning_recall_threshold() -> f32 {
    0.45
}

fn default_self_learning_min_importance() -> f32 {
    0.4
}

fn default_self_learning_approved_importance() -> f32 {
    0.7
}

fn default_self_learning_provisional_importance() -> f32 {
    0.5
}

fn default_self_learning_tick_secs() -> u64 {
    60
}

fn default_self_learning_max_items_per_run() -> u32 {
    50
}

fn default_self_learning_default_freshness_days() -> u32 {
    30
}

fn default_self_learning_experiential_lookback_secs() -> u64 {
    604800
}

fn default_self_learning_max_rounds() -> u32 {
    24
}

fn default_self_learning_deep_max_children_per_run() -> u32 {
    8
}

impl Default for SelfLearningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_self_learning_backend(),
            db_path: default_self_learning_db_path(),
            model: None,
            source: CurationSource::default(),
            cc_allow_failover_to_models: false,
            recall_top_k: default_self_learning_recall_top_k(),
            recall_threshold: default_self_learning_recall_threshold(),
            min_importance: default_self_learning_min_importance(),
            approved_importance: default_self_learning_approved_importance(),
            provisional_importance: default_self_learning_provisional_importance(),
            surface_provisional: true,
            tick_secs: default_self_learning_tick_secs(),
            max_items_per_run: default_self_learning_max_items_per_run(),
            default_freshness_days: default_self_learning_default_freshness_days(),
            experiential_lookback_secs: default_self_learning_experiential_lookback_secs(),
            max_rounds: default_self_learning_max_rounds(),
            deep_max_children_per_run: default_self_learning_deep_max_children_per_run(),
        }
    }
}

/// Doc-curator configuration. Every install must opt in: there is no
/// default tier because the right model depends on the install's
/// available providers (driftwerk: OpenRouter, Harbor: Bedrock,
/// etc.). The curator errors with a clear message if neither this
/// section nor the CLI `--tier` override is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Tier spec passed straight to `TierRegistry::resolve_routed_model`.
    /// Examples: `"openrouter:deepseek:powerful"`, `"anthropic:powerful"`,
    /// `"bedrock.anthropic:powerful"`.
    #[serde(default)]
    pub tier: Option<String>,
    /// Cap on each reply while editing a page. The curator edits a page
    /// passage by passage, so a reply holds edits, never the whole page; a
    /// reply that reaches this cap is discarded rather than applied.
    #[serde(default = "default_curator_max_tokens")]
    pub max_tokens: u32,
    /// Cap on each notes reply when a page's sources are read in chunks. A
    /// reply that reaches it is never used: its chunk is halved and read
    /// again.
    #[serde(default = "default_curator_notes_max_tokens")]
    pub notes_max_tokens: u32,
    /// Replies the curator may take to edit one page before it gives the
    /// page up, unchanged.
    #[serde(default = "default_curator_max_turns")]
    pub max_turns: u32,
    /// Safe-input budget for the configured tier (rough token estimate
    /// via chars/3). When a page's prompt would exceed this, the
    /// curator switches to chunked map-reduce: extract notes per
    /// source-file chunk, then edit the page from the combined notes.
    /// Default sized conservatively for DeepSeek's 163,840-token window.
    #[serde(default = "default_curator_context_safety_tokens")]
    pub context_safety_tokens: u32,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            tier: None,
            max_tokens: default_curator_max_tokens(),
            notes_max_tokens: default_curator_notes_max_tokens(),
            max_turns: default_curator_max_turns(),
            context_safety_tokens: default_curator_context_safety_tokens(),
        }
    }
}

fn default_curator_max_tokens() -> u32 {
    8_000
}

fn default_curator_notes_max_tokens() -> u32 {
    4_000
}

fn default_curator_max_turns() -> u32 {
    30
}

fn default_curator_context_safety_tokens() -> u32 {
    140_000
}

/// Configuration for trace capture (LLM fine-tuning data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TracingConfig {
    /// Whether trace capture is enabled. Default: true.
    #[serde(default = "default_tracing_enabled")]
    pub enabled: bool,
    /// Override directory for trace files. Default: `{data_dir}/traces`.
    #[serde(default)]
    pub traces_dir: Option<String>,
    /// Days of trace files to keep; `0` keeps them forever. Traces are
    /// write-only diagnostics that nothing reads back, and without a window
    /// they fill the volume holding the vault and every agent workspace.
    #[serde(default = "default_trace_retention_days")]
    pub retention_days: u32,
}

fn default_tracing_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    14
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            enabled: default_tracing_enabled(),
            traces_dir: None,
            retention_days: default_trace_retention_days(),
        }
    }
}

/// Which backend the advisor calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorSource {
    /// Direct LLM call routed through the agent's `LlmProvider`
    /// (and therefore the existing failover chain).
    #[default]
    Model,
    /// Spawn the Claude Code CLI as a one-shot read-only advisor.
    /// Subscription-friendly path. Per Anthropic TOS Max OAuth tokens may
    /// only flow through the CLI subprocess, never the Messages API.
    ClaudeCode,
    /// Dispatch to another configured agent (must have `advisor_mode = true`).
    Agent,
}

/// Payload shape forwarded to the advisor.
///
/// `Transcript` ships the message history verbatim so the advisor sees what
/// the agent saw. `Summary` ships a compressed digest — useful when the
/// transcript is huge and the operator wants to cap forwarded tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorPayload {
    #[default]
    Transcript,
    Summary,
}

/// Advisor configuration (global section).
///
/// The advisor is a voluntary "second opinion" surface. The main agent may
/// invoke an `advisor()` tool that forwards the in-scope session to a
/// configured reviewer and returns written advice. The reviewer is
/// **advisory only** — it must never edit, write, or execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorConfig {
    /// Enable the advisor tool for agents that inherit this config. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Which backend to call.
    #[serde(default)]
    pub source: AdvisorSource,
    /// Payload shape forwarded to the advisor. Default `Transcript` —
    /// even for paid models, paying once to ship the full context beats
    /// paying repeatedly for the advisor to round-trip through "give me
    /// a summary, now go fetch the file I summarised out."
    #[serde(default)]
    pub payload: AdvisorPayload,
    /// Token cap on the forwarded payload. When the transcript exceeds
    /// this, the advisor sees a tail-preferred truncation with a
    /// `[truncated]` marker. Default 200_000.
    #[serde(default = "default_advisor_payload_max_tokens")]
    pub payload_max_tokens: u32,
    /// How many advisor invocations may run per user turn. Excess
    /// invocations return `TurnLimitExceeded`. Default 3.
    #[serde(default = "default_advisor_per_turn_limit")]
    pub per_turn_invocation_limit: u32,
    /// Model the advisor runs on.
    ///
    /// For `source = "model"`: resolved via the agent's `TierRegistry`,
    /// so tier keys (`"cheap"`/`"standard"`/`"powerful"`) and concrete
    /// model ids both work.
    ///
    /// For `source = "claude_code"`: forwarded verbatim to the CLI's
    /// `--model` flag. CC accepts its own aliases (`"haiku"`/`"sonnet"`/
    /// `"opus"`) which resolve to the latest available version, or any
    /// concrete model id CC understands. When unset, CC uses its own
    /// default.
    #[serde(default)]
    pub model: Option<String>,
    /// Cost-guard for `source = "claude_code"`. When false (default), CC
    /// failures are surfaced to the user with `FailoverDisabled`; the advisor
    /// does NOT silently fall back to paid models. When true, falls back to
    /// `cc_fallback_model` via the standard failover chain.
    #[serde(default)]
    pub cc_allow_failover_to_models: bool,
    /// Model used when `cc_allow_failover_to_models = true` and CC fails.
    #[serde(default)]
    pub cc_fallback_model: Option<String>,
    /// Max conversational rounds the advisor's tool loop may take, across
    /// all sources (Model, Agent, Claude Code). Each tool call consumes a
    /// round, so the read-only allowlist requires headroom to verify
    /// premises before synthesising. Default 7 — enough room for 1–2
    /// premise-verification reads plus the synthesis turn.
    /// Accepts the legacy `cc_max_turns` key for back-compat.
    #[serde(default = "default_advisor_max_rounds", alias = "cc_max_turns")]
    pub max_rounds: u32,
    /// `--effort` passed to CC. Default `"high"` — advisor benefits from
    /// reasoning headroom.
    #[serde(default = "default_advisor_cc_effort")]
    pub cc_effort: Option<String>,
    /// Target agent id for `source = "agent"`. The target must have
    /// `advisor_mode = true` set on its `AgentConfig`.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Append a custom suffix to the advisor system prompt. Useful for
    /// project-specific guidance ("focus on security review", etc.).
    /// The advisory-only contract is appended regardless of this field.
    #[serde(default)]
    pub system_prompt_suffix: Option<String>,
    /// Fan-out ("panel") settings. Controls the multi-lens parallel review
    /// the calling agent can opt into by passing 2+ `lenses` to the tool.
    #[serde(default)]
    pub fanout: AdvisorFanoutConfig,
}

/// Fan-out ("panel") settings for the advisor.
///
/// When the calling agent passes 2+ distinct `lenses`, the advisor reviews
/// the same session once per lens — each a separate read-only advisor
/// session running in parallel — then a final reconciling advisor merges
/// them into one opinion that is handed back exactly like a single review.
/// Single-review stays the default: fan-out only triggers when the caller
/// supplies lenses. Fan-out honours the configured backend, so a strong
/// reviewer fans out on the strong reviewer: `claude_code` spawns parallel CLI
/// subprocesses (legs and synth on the configured Claude Code model), `model`/
/// `agent` run in-process loops. A weaker/non-Claude model is used only when
/// explicitly configured (`synth_model`) or via the `cc_allow_failover_to_models`
/// guard — fan-out never silently downgrades.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorFanoutConfig {
    /// Hard kill-switch. When false the advisor always runs a single review
    /// and ignores the `lenses` argument. Default true (the effective gate is
    /// still the caller supplying lenses).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cap on parallel lens sessions per call. Excess lenses are dropped and
    /// the calling agent is told how many actually ran. Default 4. Note each
    /// lens is a full reviewer session, so on `claude_code` this many CLI
    /// subprocesses run at once.
    #[serde(default = "default_advisor_fanout_max_lenses")]
    pub max_lenses: u32,
    /// Explicit model for the reconciling synthesiser (the escape hatch to
    /// reconcile on a specific/cheaper model). When unset the synth stays on
    /// the configured backend's reviewer: the Claude Code model for
    /// `source = "claude_code"`, the `standard` tier (then the per-lens model)
    /// for `source = "model"`, and the target agent's default for
    /// `source = "agent"`.
    #[serde(default)]
    pub synth_model: Option<String>,
    /// Max concurrent Claude Code subprocesses for THIS agent, across EVERY CC
    /// surface — advisor (lens legs + synth) AND `coding.fanout` coders. This is
    /// the shared FAIRNESS limit: it stops one agent monopolising the host even
    /// when it runs an advisor panel and a coding fan-out at once. `0` disables
    /// it (unbounded for the agent). Hot-reloads via the per-agent `config.toml`
    /// watcher. Default 4: a single advisor panel (≤ `max_lenses`) runs fully
    /// parallel, while a second concurrent CC call for the same agent queues.
    /// Only Claude Code spawns subprocesses; the in-process advisor
    /// `model`/`agent` backends are unaffected. Keep this ≥ `max_lenses`:
    /// setting it lower makes a single panel's own legs contend for slots, and a
    /// queued leg burns its timeout waiting (it can self-degrade). The default
    /// (4 = `max_lenses`) avoids that. (Field lives under `[advisor.fanout]` for
    /// historical reasons but governs all CC subprocesses.)
    #[serde(default = "default_advisor_fanout_max_concurrent")]
    pub max_concurrent: u32,
    /// Hub-wide ceiling on concurrent Claude Code subprocesses across ALL agents
    /// and EVERY CC surface (advisor + `coding.fanout`) — the host-safety
    /// backstop (the real OOM guard, and the one shared budget that stops the
    /// advisor and coding fan-outs jointly exceeding the host's memory). `0`
    /// disables it. Read ONLY from the global `[advisor.fanout]`; per-agent
    /// overrides of this field are ignored. Restart to change (global config).
    /// Default 8. Acquired together with `max_concurrent` (agent-gate then
    /// hub-gate), so the per-agent caps need NOT sum to this — the hub gate is
    /// the hard ceiling regardless, and the per-agent gate only bounds any one
    /// agent's share.
    #[serde(default = "default_advisor_fanout_hub_max_concurrent")]
    pub hub_max_concurrent_cc: u32,
}

fn default_advisor_fanout_max_lenses() -> u32 {
    4
}

fn default_advisor_fanout_max_concurrent() -> u32 {
    4
}

fn default_advisor_fanout_hub_max_concurrent() -> u32 {
    8
}

impl Default for AdvisorFanoutConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lenses: default_advisor_fanout_max_lenses(),
            synth_model: None,
            max_concurrent: default_advisor_fanout_max_concurrent(),
            hub_max_concurrent_cc: default_advisor_fanout_hub_max_concurrent(),
        }
    }
}

fn default_advisor_payload_max_tokens() -> u32 {
    200_000
}

fn default_advisor_per_turn_limit() -> u32 {
    3
}

fn default_advisor_max_rounds() -> u32 {
    7
}

fn default_advisor_cc_effort() -> Option<String> {
    Some("high".to_string())
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: AdvisorSource::default(),
            payload: AdvisorPayload::default(),
            payload_max_tokens: default_advisor_payload_max_tokens(),
            per_turn_invocation_limit: default_advisor_per_turn_limit(),
            model: None,
            cc_allow_failover_to_models: false,
            cc_fallback_model: None,
            max_rounds: default_advisor_max_rounds(),
            cc_effort: default_advisor_cc_effort(),
            agent_id: None,
            system_prompt_suffix: None,
            fanout: AdvisorFanoutConfig::default(),
        }
    }
}

/// Per-agent advisor overrides. All fields are `Option` — unset fields
/// inherit from the global `[advisor]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentAdvisorConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub source: Option<AdvisorSource>,
    #[serde(default)]
    pub payload: Option<AdvisorPayload>,
    #[serde(default)]
    pub payload_max_tokens: Option<u32>,
    #[serde(default)]
    pub per_turn_invocation_limit: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cc_allow_failover_to_models: Option<bool>,
    #[serde(default)]
    pub cc_fallback_model: Option<String>,
    #[serde(default, alias = "cc_max_turns")]
    pub max_rounds: Option<u32>,
    #[serde(default)]
    pub cc_effort: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub system_prompt_suffix: Option<String>,
    /// Override the whole fan-out block. Unset inherits the base block.
    #[serde(default)]
    pub fanout: Option<AdvisorFanoutConfig>,
}

/// Hub server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubConfig {
    /// HTTP listen address for admin API.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Data directory for persistence.
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Optional fast-local-disk root for ephemeral coding worktrees and build
    /// output. When set, `{worktree_root}/worktrees/` holds the `code.*`
    /// checked-out trees (their `{data_dir}/worktree-bases/` mirrors stay
    /// put), and `{worktree_root}/work/` holds each project's repo-backed
    /// work sessions (`coding.session`, `coding.fanout`,
    /// `projects.open_workspace`) together with their bare bases, so no git
    /// worktree registry is ever shared between instances. Scratch benches
    /// stay under `data_dir`. Point this at instance-local disk: build I/O is
    /// dominated by small-file latency, so an EFS/NFS `data_dir` makes
    /// installs and builds many times slower. Setting it on an install that
    /// already has work sessions restarts each persisted coding session once,
    /// exactly as a redeploy does. Unset → everything lives under `data_dir`.
    #[serde(default)]
    pub worktree_root: Option<String>,
    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Path to the policy YAML file. If not set, auto-detects `policies/default.yaml`.
    #[serde(default)]
    pub policy_path: Option<String>,
    /// Graceful shutdown drain timeout in seconds. Default: 60.
    /// On SIGTERM, the process waits this long for active agent runners to complete.
    #[serde(default)]
    pub drain_timeout_secs: Option<u64>,
    /// Seconds to wait after boot before resuming interrupted sessions
    /// (default 180). Outlives a rolling deploy's stop grace period so a
    /// booting instance never resumes turns still draining on the outgoing
    /// one.
    pub resume_delay_secs: Option<u64>,
    /// Workflow maintenance check interval in seconds. Default: 300 (5 min).
    /// A periodic loop checks for orphaned workflow instances (e.g. after a restart)
    /// and re-spawns their execution loops.
    #[serde(default)]
    pub workflow_maintenance_interval_secs: Option<u64>,
    /// Externally-reachable base URL for the gateway, used by the MCP
    /// preset catalog to compute the OAuth callback URL operators
    /// should paste into their vendor app registration, and by the
    /// webhook receiver to rebuild the full request URL that HubSpot
    /// signatures cover. Format: `https://hub.example.com` (no trailing
    /// slash). Optional; when unset the UI shows a placeholder reminding
    /// the operator to configure it and HubSpot webhooks cannot verify.
    #[serde(default)]
    pub public_url: Option<String>,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            worktree_root: None,
            log_level: default_log_level(),
            policy_path: None,
            drain_timeout_secs: None,
            resume_delay_secs: None,
            workflow_maintenance_interval_secs: None,
            public_url: None,
        }
    }
}

impl HubConfig {
    /// `worktree_root` as a path, if configured (see the field docs).
    pub fn worktree_root_path(&self) -> Option<&std::path::Path> {
        self.worktree_root.as_deref().map(std::path::Path::new)
    }
}

/// Local agents configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalAgentsConfig {
    /// Whether local agents support is enabled.
    #[serde(default)]
    pub enabled: bool,
}

/// NATS connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsConfig {
    /// NATS server URL. Schemes: `nats://` (cleartext), `tls://` (TLS),
    /// `ws://` / `wss://` (WebSocket). Comma-separated for clusters.
    #[serde(default = "default_nats_url")]
    pub url: String,
    /// Path to a NATS `.creds` file (NKey + JWT auth). The standard
    /// secure-credentials format produced by `nsc add user ... --creds-out`.
    /// Mutually exclusive with `token` — if both are set, `credentials`
    /// wins.
    #[serde(default)]
    pub credentials: Option<String>,
    /// Bearer token for authentication. Suitable for simple deployments
    /// where the operator manages a single shared secret. For production
    /// deployments prefer `credentials` (NKey + JWT), which supports
    /// rotation and per-component identities.
    #[serde(default)]
    pub token: Option<String>,
    /// Require TLS regardless of URL scheme. When the URL already uses
    /// `tls://` this is redundant. Set true to harden a `nats://` URL or
    /// to forbid plaintext fall-back on a mixed cluster.
    #[serde(default)]
    pub tls: bool,
    /// JetStream replica count for the workforce streams and the
    /// leader-election bucket. Leave 1 (default) on a single NATS server —
    /// it rejects higher values. Set 3 on a 3-node NATS cluster so streams
    /// and the leader lease survive a node loss.
    #[serde(default = "default_nats_stream_replicas")]
    pub stream_replicas: usize,
}

fn default_nats_stream_replicas() -> usize {
    1
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: default_nats_url(),
            credentials: None,
            token: None,
            tls: false,
            stream_replicas: default_nats_stream_replicas(),
        }
    }
}

/// Multi-instance expectations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Declare that this deployment runs (or may run) more than one hub
    /// instance. When true, boot FAILS if JetStream leader election cannot be
    /// set up — an ungated instance would run every singleton (scheduler,
    /// heartbeat, Slack ingress) alongside its peers. Leave false (default)
    /// for single-instance deployments, where running ungated is correct.
    #[serde(default)]
    pub expected: bool,
}

/// Weaviate connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeaviateConfig {
    /// Weaviate HTTP URL.
    #[serde(default = "default_weaviate_url")]
    pub url: String,
    /// Whether to use Weaviate (false = in-memory store).
    #[serde(default)]
    pub enabled: bool,
    /// API key for Weaviate authentication.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Auto-rebuild the `WorkforceMemory` class on startup if its schema
    /// drifted (e.g. after a redeploy that changed property types). Exports
    /// objects to a backup file under `{hub.data_dir}/memory-backups/` then
    /// recreates the class with the current schema and re-ingests.
    #[serde(default = "default_true")]
    pub auto_rebuild_on_schema_drift: bool,

    /// How long startup keeps retrying Weaviate readiness before failing
    /// closed. Long enough to ride out a cold cluster-wide restart where
    /// Weaviate is still coming up alongside the hub.
    #[serde(default = "crate::postgres::default_backend_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
}

impl Default for WeaviateConfig {
    fn default() -> Self {
        Self {
            url: default_weaviate_url(),
            enabled: false,
            api_key: None,
            auto_rebuild_on_schema_drift: true,
            connect_timeout_secs: crate::postgres::default_backend_connect_timeout_secs(),
        }
    }
}

/// vellev connection configuration (`[vellev]`), used when any of `[memory]
/// backend`, `[artifacts] index_backend` and `[knowledge] vector_backend`
/// is `"vellev"`, which needs `api_key` and `tenant`. Each of the three is
/// chosen apart from the others.
///
/// Whichever of memories, artifacts and code vectors are kept in vellev
/// are kept in the tenant. Artifacts and code are always embedded by the
/// node's own model (bge-base-en-v1.5), as memories are unless
/// `[embeddings]` has the hub embed them, so the node must run it: on one
/// that does not, writes and searches of those collections fail, saying
/// the node cannot use its embedding model.
#[derive(Clone, Serialize, Deserialize)]
pub struct VellevConfig {
    /// A vellev node's HTTP URL; by default port 18080 on this host, clear
    /// of Weaviate's 8080.
    #[serde(default = "default_vellev_url")]
    pub url: String,
    /// The key presented to the node: a key for the tenant, or an admin key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// The tenant everything is kept in. It is never created by workforce:
    /// an operator creates it, so that a mistyped name cannot start an
    /// empty store.
    #[serde(default)]
    pub tenant: String,
    /// How long startup keeps retrying the node before failing closed.
    #[serde(default = "crate::postgres::default_backend_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// How a store set to `"shadow"` asks vellev beside Weaviate
    /// (`[vellev.shadow]`, DW-676). Boxed: `WorkforceConfig` is copied
    /// onto the stack in deep async call chains, and some tests run near
    /// a thread's 2 MiB stack in debug builds. Inline, these 288 bytes
    /// overflowed one (`coding_rounds::a_rejected_step_is_corrected_...`).
    #[serde(default)]
    pub shadow: Box<VellevShadowConfig>,
}

impl Default for VellevConfig {
    fn default() -> Self {
        Self {
            url: default_vellev_url(),
            api_key: None,
            tenant: String::new(),
            connect_timeout_secs: crate::postgres::default_backend_connect_timeout_secs(),
            shadow: Box::default(),
        }
    }
}

impl std::fmt::Debug for VellevConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VellevConfig")
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("tenant", &self.tenant)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("shadow", &self.shadow)
            .finish()
    }
}

fn default_vellev_url() -> String {
    "http://localhost:18080".to_string()
}

/// `[memory] backend` and `[artifacts] index_backend`: Weaviate serves, and
/// vellev is written and asked beside it and compared (DW-676).
pub const SHADOWED: &str = "shadow";

/// The stores shadow mode can shadow. Code vectors are not among them: by
/// the owner's decision of 2026-10-12 they stay on Weaviate, unshadowed,
/// until vellev's memory use for them can be cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowedStore {
    Memories,
    Artifacts,
}

impl ShadowedStore {
    /// Its table under `[vellev.shadow]`.
    pub fn table(self) -> &'static str {
        match self {
            Self::Memories => "[vellev.shadow.memories]",
            Self::Artifacts => "[vellev.shadow.artifacts]",
        }
    }
}

/// `[vellev.shadow]`: how each shadowed store's reads are asked of vellev
/// in the background (see `crate::shadow`). Read once at startup.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VellevShadowConfig {
    /// The kill switch at startup: while set, vellev is asked nothing.
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub memories: ShadowStoreConfig,
    #[serde(default)]
    pub artifacts: ShadowStoreConfig,
    /// How often the leader brings vellev's replica of memories in line
    /// with Weaviate, in seconds (default 900; 60 to 86400).
    pub memory_reconcile_secs: Option<u64>,
    /// The leader's nightly known-item runs of memories.
    #[serde(default)]
    pub known_items: KnownItemsConfig,
    /// The opt-in LLM judge of artifacts' known items.
    #[serde(default)]
    pub judge: JudgeConfig,
    /// The memory agreement gate's thresholds (exit criterion M2), as the
    /// owner signed them before day 1. Left out, the report judges no
    /// agreement: it says "thresholds not set", never pass.
    #[serde(default)]
    pub gates: GatesConfig,
    /// Keys this hub does not know: kept so they can be warned of by name,
    /// never refused, so that a rollback to an older hub still starts.
    #[serde(flatten, default, skip_serializing)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// One store's `[vellev.shadow.*]` table. Each value left out is the
/// store's default ([`VellevShadowConfig::settings`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShadowStoreConfig {
    /// Share of reads also asked of vellev, 0 to 1 (default 1).
    pub sample: Option<f64>,
    /// Shadow reads at once; a read finding none free is not shadowed
    /// (default 4).
    pub permits: Option<usize>,
    /// How long a shadow read may take (default 2000 for memories, 5000
    /// for artifacts).
    pub deadline_ms: Option<u64>,
    /// Failures in a row that stop shadow reads (default 3).
    pub trip_after: Option<u32>,
    /// How long they stay stopped before one is tried (default 30).
    pub rest_secs: Option<u64>,
    /// Depth the two rankings are compared to (default 10).
    pub k: Option<usize>,
    /// Rank-biased overlap's persistence, between 0 and 1 (default 0.9).
    pub rbo_p: Option<f64>,
    /// Keys this hub does not know (see [`VellevShadowConfig::unknown`]).
    #[serde(flatten, default, skip_serializing)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// `[vellev.shadow.known_items]`: the leader's nightly runs of known
/// items, memories drawn from Weaviate by a seed, each asked of both
/// stores by a query made from it, to see whether it comes back first.
/// Each value left out is the default ([`VellevShadowConfig::known_items`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KnownItemsConfig {
    /// Memories drawn a night (default 1000; 0 runs none; at most 5000).
    pub items: Option<usize>,
    /// Memories filed under no identity drawn a night, recalled as system
    /// jobs recall them, unscoped (default 250; 0 draws none; at most
    /// 5000).
    pub unscoped_items: Option<usize>,
    /// The seed each night's draw is made from, with its date (default 0).
    pub seed: Option<u64>,
    /// The UTC hour from which a night's run may start (default 2).
    pub hour_utc: Option<u32>,
    /// The UTC hour before which it must start (default 6; up to 24):
    /// past it, the run waits for the next day.
    pub until_hour_utc: Option<u32>,
    /// The pause between two items, in milliseconds (default 250; at most
    /// 10000).
    pub pace_ms: Option<u64>,
    /// Keys this hub does not know (see [`VellevShadowConfig::unknown`]).
    #[serde(flatten, default, skip_serializing)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// `[vellev.shadow.judge]`: an LLM grading what each side found for the
/// artifacts' known-item queries (design r2 section 6.5). Off unless set:
/// it reads artifacts whatever their access rules, and its calls cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JudgeConfig {
    /// Whether the judge runs (default false).
    #[serde(default)]
    pub enabled: bool,
    /// Queries judged a UTC day at most (default 200; 1 to 1000).
    pub daily_cap: Option<usize>,
    /// Keys this hub does not know (see [`VellevShadowConfig::unknown`]).
    #[serde(flatten, default, skip_serializing)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// `[vellev.shadow.gates]`: M2's thresholds for memories, each the
/// pre-week replay's mean less 0.03, never below its floor (overlap@10
/// 0.85, top-1 0.85, RBO 0.80). All three or none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GatesConfig {
    pub overlap: Option<f64>,
    pub top1: Option<f64>,
    pub rbo: Option<f64>,
    /// The non-inferiority margin of the self@1 test, as a share (default
    /// 0.005, half a point; 0 to 0.05).
    pub self_first_margin: Option<f64>,
    /// Distinct memories both stores answered a week the scoped group needs
    /// before self@1 is judged (default 600; at least 1). Fewer than can
    /// show the margin with no discordant pair is too few as well.
    pub self_first_min: Option<usize>,
    /// The same for the unscoped population (default 600; at least 1).
    pub unscoped_min: Option<usize>,
    /// Keys this hub does not know (see [`VellevShadowConfig::unknown`]).
    #[serde(flatten, default, skip_serializing)]
    pub unknown: std::collections::BTreeMap<String, toml::Value>,
}

/// The self@1 gate's settings, checked: a paired McNemar non-inferiority
/// test, one-sided at 95 %.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelfFirstGate {
    pub margin: f64,
    pub min: usize,
    pub unscoped_min: usize,
}

impl Default for SelfFirstGate {
    fn default() -> Self {
        Self {
            margin: 0.005,
            min: 600,
            unscoped_min: 600,
        }
    }
}

/// M2's thresholds, checked: each at or above its floor and at most 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AgreementGates {
    pub overlap: f64,
    pub top1: f64,
    pub rbo: f64,
}

/// [`KnownItemsConfig`] with its defaults applied and checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownItemSettings {
    /// Memories drawn a night; 0 runs none.
    pub items: usize,
    /// Memories filed under no identity drawn a night.
    pub unscoped_items: usize,
    pub seed: u64,
    /// A run starts from this UTC hour and before `until_hour_utc`.
    pub hour_utc: u32,
    pub until_hour_utc: u32,
    pub pace: std::time::Duration,
}

/// How long a known-item run may be found still running before it is
/// taken for abandoned and its day freed: past the longest run settings
/// allow ([`VellevShadowConfig::known_items`] refuses longer).
pub const KNOWN_ITEM_STALE: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

impl VellevShadowConfig {
    /// Every key under `[vellev.shadow]` this hub does not know, each with
    /// the table it is in: `("[vellev.shadow.memories]", "sample_rate")`,
    /// or `("[vellev.shadow]", "memory")` for a misspelled table.
    pub fn unknown_keys(&self) -> Vec<(&'static str, String)> {
        let mut keys: Vec<(&'static str, String)> = self
            .unknown
            .keys()
            .map(|k| ("[vellev.shadow]", k.clone()))
            .collect();
        for (table, config) in [
            ("[vellev.shadow.memories]", &self.memories),
            ("[vellev.shadow.artifacts]", &self.artifacts),
        ] {
            keys.extend(config.unknown.keys().map(|k| (table, k.clone())));
        }
        keys.extend(
            self.known_items
                .unknown
                .keys()
                .map(|k| ("[vellev.shadow.known_items]", k.clone())),
        );
        keys.extend(
            self.judge
                .unknown
                .keys()
                .map(|k| ("[vellev.shadow.judge]", k.clone())),
        );
        keys.extend(
            self.gates
                .unknown
                .keys()
                .map(|k| ("[vellev.shadow.gates]", k.clone())),
        );
        keys
    }

    /// M2's thresholds, `None` when none is set; `Err` naming a value that
    /// cannot be used: one below its floor or above 1, or a set of fewer
    /// than all three.
    pub fn gates(&self) -> eyre::Result<Option<AgreementGates>> {
        let g = &self.gates;
        let table = "[vellev.shadow.gates]";
        let (overlap, top1, rbo) = match (g.overlap, g.top1, g.rbo) {
            (None, None, None) => return Ok(None),
            (Some(overlap), Some(top1), Some(rbo)) => (overlap, top1, rbo),
            _ => eyre::bail!(
                "{table} needs all of overlap, top1 and rbo, or none: M2's thresholds are signed as a set"
            ),
        };
        for (name, value, floor) in [
            ("overlap", overlap, 0.85),
            ("top1", top1, 0.85),
            ("rbo", rbo, 0.80),
        ] {
            if !(floor..=1.0).contains(&value) {
                eyre::bail!(
                    "{table} {name} must be from its floor {floor} to 1, not {value}: M2's threshold is the replay's mean less 0.03, never below the floor"
                );
            }
        }
        Ok(Some(AgreementGates { overlap, top1, rbo }))
    }

    /// The self@1 gate's margin and minimum sizes, or `Err` naming a value
    /// that cannot be used.
    pub fn self_first_gate(&self) -> eyre::Result<SelfFirstGate> {
        let g = &self.gates;
        let table = "[vellev.shadow.gates]";
        let d = SelfFirstGate::default();
        let margin = g.self_first_margin.unwrap_or(d.margin);
        if !(0.0..=0.05).contains(&margin) {
            eyre::bail!(
                "{table} self_first_margin must be from 0 to 0.05, not {margin}: how far below Weaviate's self@1 vellev's may be, as a share"
            );
        }
        let min = g.self_first_min.unwrap_or(d.min);
        let unscoped_min = g.unscoped_min.unwrap_or(d.unscoped_min);
        for (name, value) in [("self_first_min", min), ("unscoped_min", unscoped_min)] {
            if value == 0 {
                eyre::bail!(
                    "{table} {name} must be at least 1: the items both stores answered a week before self@1 is judged"
                );
            }
        }
        Ok(SelfFirstGate {
            margin,
            min,
            unscoped_min,
        })
    }

    /// The judge's daily cap when it is on, `None` when off; `Err` naming
    /// a cap that cannot be used.
    pub fn judge_cap(&self) -> eyre::Result<Option<usize>> {
        let cap = self.judge.daily_cap.unwrap_or(200);
        if !(1..=1000).contains(&cap) {
            eyre::bail!(
                "[vellev.shadow.judge] daily_cap must be from 1 to 1000, not {cap}: the artifact known-item queries an LLM judges a UTC day"
            );
        }
        Ok(self.judge.enabled.then_some(cap))
    }

    /// The known-item runs' settings, or `Err` naming the value that cannot
    /// be used.
    ///
    /// Each pool at most 5,000 items. A run takes at least its items times
    /// `pace` (each item also costs its embedding and recalls), so settings
    /// whose items times `pace` reach [`KNOWN_ITEM_STALE`] are refused: such
    /// a run would be taken for abandoned while still under way.
    pub fn known_items(&self) -> eyre::Result<KnownItemSettings> {
        let c = &self.known_items;
        let table = "[vellev.shadow.known_items]";
        let items = c.items.unwrap_or(1000);
        if items > 5_000 {
            eyre::bail!(
                "{table} items must be from 0 to 5000, not {items}: the memories each night's known-item run draws (0 runs none)"
            );
        }
        let unscoped_items = c.unscoped_items.unwrap_or(250);
        if unscoped_items > 5_000 {
            eyre::bail!(
                "{table} unscoped_items must be from 0 to 5000, not {unscoped_items}: the memories filed under no identity each night's run draws"
            );
        }
        let hour_utc = c.hour_utc.unwrap_or(2);
        if hour_utc > 23 {
            eyre::bail!(
                "{table} hour_utc must be from 0 to 23, not {hour_utc}: the UTC hour from which the night's known-item run may start"
            );
        }
        let until_hour_utc = c.until_hour_utc.unwrap_or(6);
        if until_hour_utc > 24 || until_hour_utc <= hour_utc {
            eyre::bail!(
                "{table} until_hour_utc must be after hour_utc ({hour_utc}) and at most 24, not {until_hour_utc}: the UTC hour before which the night's run must start"
            );
        }
        let pace_ms = c.pace_ms.unwrap_or(250);
        if pace_ms > 10_000 {
            eyre::bail!(
                "{table} pace_ms must be from 0 to 10000, not {pace_ms}: the pause between two known items"
            );
        }
        let pace = std::time::Duration::from_millis(pace_ms);
        let least = pace.saturating_mul(u32::try_from(items + unscoped_items).unwrap_or(u32::MAX));
        if least >= KNOWN_ITEM_STALE {
            eyre::bail!(
                "{table} pace_ms of {pace_ms} with {items} and {unscoped_items} items makes a run of at least {least:?}, past the {KNOWN_ITEM_STALE:?} after which a run is taken for abandoned: lower pace_ms or the items"
            );
        }
        Ok(KnownItemSettings {
            items,
            unscoped_items,
            seed: c.seed.unwrap_or(0),
            hour_utc,
            until_hour_utc,
            pace,
        })
    }

    /// How often memories' replica is reconciled, or `Err` naming the
    /// value that cannot be used: at least a minute, so a run is over
    /// before the next is due, and at most a day, so what was missed is not
    /// left for longer.
    pub fn memory_reconcile(&self) -> eyre::Result<std::time::Duration> {
        let secs = self.memory_reconcile_secs.unwrap_or(900);
        if !(60..=86_400).contains(&secs) {
            return Err(eyre::eyre!(
                "[vellev.shadow] memory_reconcile_secs must be from 60 to 86400, not {secs}: how often vellev's replica of memories is brought in line with Weaviate"
            ));
        }
        Ok(std::time::Duration::from_secs(secs))
    }

    /// `store`'s settings for the kernel, or `Err` naming its table and the
    /// value that cannot be used.
    ///
    /// The defaults are design revision 2's: every read sampled (A13), four
    /// at a time, 2 s for memories and 5 s for artifacts, compared to depth
    /// 10 (the exit criteria's overlap@10) with p 0.9. The breaker's are
    /// those of DW-620's artifact search (`SEARCH_TRIPS_AFTER`,
    /// `SEARCH_REST`).
    pub fn settings(&self, store: ShadowedStore) -> eyre::Result<crate::shadow::ShadowSettings> {
        let (table, deadline_ms) = match store {
            ShadowedStore::Memories => (&self.memories, 2_000),
            ShadowedStore::Artifacts => (&self.artifacts, 5_000),
        };
        crate::shadow::ShadowSettings {
            sample: table.sample.unwrap_or(1.0),
            permits: table.permits.unwrap_or(4),
            deadline: std::time::Duration::from_millis(table.deadline_ms.unwrap_or(deadline_ms)),
            trip_after: table.trip_after.unwrap_or(3),
            rest: std::time::Duration::from_secs(table.rest_secs.unwrap_or(30)),
            k: table.k.unwrap_or(10),
            rbo_p: table.rbo_p.unwrap_or(0.9),
        }
        .checked()
        .map_err(|e| eyre::eyre!("{}: {e}", store.table()))
    }
}

/// Which agents' memories an agent admits through automatic recall.
///
/// `Visibility::Private` is scoped to the owning *user*, not the owning agent
/// (DW-525), so by default a user's agents share their memories. In a
/// multi-agent deployment that lets one agent's working context bleed into
/// another's prompt; this scope reins that in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRecallScope {
    /// The agent's own memories plus any shared beyond their owner
    /// (team/global visibility). Other agents' private memories are excluded.
    #[default]
    Shared,
    /// Only memories this agent owns.
    Isolated,
}

impl AgentRecallScope {
    /// Whether recall must be confined to the requesting agent's own memories
    /// (a server-side `owner` filter), versus also admitting shared memories
    /// through a post-filter.
    pub fn owned_only(self) -> bool {
        matches!(self, Self::Isolated)
    }
}

/// Memory store configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Backend: "sqlite" (default), "weaviate", "vellev", or "in_memory".
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    /// Path to the SQLite database file (only for sqlite backend).
    #[serde(default = "default_memory_db_path")]
    pub db_path: String,
    /// Embedding provider: "openai" or "none" for keyword-only search.
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    /// Embedding model name (e.g., "text-embedding-3-small").
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Embedding API URL (OpenAI-compatible).
    #[serde(default = "default_embedding_api_url")]
    pub embedding_api_url: String,
    /// API key for the embedding provider: the last resort after the global
    /// vault's `embeddings/memory/api_key`. Filled from
    /// WORKFORCE_EMBEDDING_API_KEY or OPENAI_API_KEY when unset.
    #[serde(default)]
    pub embedding_api_key: Option<String>,
    /// Enable passive post-turn memory extraction (default false).
    /// When enabled, a lightweight LLM call runs after each substantive turn
    /// to extract facts worth remembering.
    #[serde(default)]
    pub passive: bool,
    /// Model for passive extraction (e.g. "cheap"). Falls back to
    /// compaction_model, then the agent's default model.
    #[serde(default)]
    pub passive_model: Option<String>,
    /// Minimum token count in the user+assistant turn to trigger passive
    /// extraction. Turns below this threshold are skipped (default 200).
    #[serde(default = "default_passive_min_turn_tokens")]
    pub passive_min_turn_tokens: u32,
    /// Custom system prompt for passive extraction.
    #[serde(default)]
    pub passive_prompt: Option<String>,
    /// Inject a dated digest of the user's recent episodic memories when a
    /// conversational-channel session opens with no dialogue yet — a fresh
    /// chat or a stale-rollover successor (default true). This is the
    /// cross-channel continuity bridge: without it a voice/chat session
    /// starts blind to what the same user discussed elsewhere. Identity-
    /// scoped; users without memories get nothing.
    #[serde(default = "default_true")]
    pub resume_context: bool,
    /// Enable per-user relationship tracking (default false). When on, each
    /// substantive human DM turn is scored for sentiment and reaction, and the
    /// rolled-up relationship is surfaced to the agent for acknowledgement.
    #[serde(default)]
    pub relationship: bool,
    /// Minimum user+assistant turn tokens to score for relationship tracking
    /// (default 40 — short emotional turns like "perfect, thanks!" are signal).
    #[serde(default = "default_relationship_min_turn_tokens")]
    pub relationship_min_turn_tokens: u32,
    /// Enable periodic memory consolidation (default false).
    /// A scheduled job merges related episodic memories into semantic
    /// memories and deduplicates.
    #[serde(default)]
    pub consolidation: bool,
    /// Consolidation interval in seconds (default 14400 = 4 hours).
    #[serde(default = "default_consolidation_interval")]
    pub consolidation_interval_secs: u64,
    /// Lookback window for session scanning during consolidation (default 43200 = 12 hours).
    /// Only sessions updated within this window are scanned for memorable facts.
    #[serde(default = "default_consolidation_lookback")]
    pub consolidation_lookback_secs: u64,
    /// Model for the consolidation merge phase (e.g. "cheap").
    /// If None, uses the agent's default model.
    #[serde(default)]
    pub consolidation_model: Option<String>,
    /// Engine for the consolidation turn (`"model"` or `"claude_code"`).
    /// `claude_code` runs a toolless CLI subprocess (flat-rate under a
    /// subscription): the hub prefetches the identity's episodic memories,
    /// embeds them in the prompt, and applies the returned JSON op plan
    /// in-process. Passive extraction always stays on `passive_model` —
    /// a subprocess per inbound message is not viable.
    #[serde(default)]
    pub consolidation_source: CurationSource,
    /// When Claude Code is unavailable (no OAuth token / CLI missing / a
    /// failed run) fall back to the `Model` engine instead of skipping the
    /// consolidation run. Off by default, mirroring
    /// `[self_learning] cc_allow_failover_to_models`; skips are logged.
    #[serde(default)]
    pub consolidation_cc_failover: bool,
    /// Enable trigger-conditioned lesson extraction from failure→recovery /
    /// user-correction turns (default false). Write-side only: lessons
    /// accumulate but do not affect the system prompt until `lesson_inject`
    /// is also enabled.
    #[serde(default)]
    pub lesson_extract: bool,
    /// Model for lesson extraction (e.g. "cheap"). Falls back to
    /// passive_model, then compaction_model, then the agent's default.
    #[serde(default)]
    pub lesson_extract_model: Option<String>,
    /// Custom system prompt for lesson extraction.
    #[serde(default)]
    pub lesson_extract_prompt: Option<String>,
    /// Inject recalled lessons into the system prompt (default false).
    /// Independent of `lesson_extract`: enable extraction first so the corpus
    /// warms up before flipping injection on.
    #[serde(default)]
    pub lesson_inject: bool,
    /// Maximum number of lessons rendered in the system prompt per turn
    /// (default 3). The recall pulls 3× this many to survive the score /
    /// confidence post-filters, then truncates back to this limit.
    #[serde(default = "default_lesson_inject_limit")]
    pub lesson_inject_limit: usize,
    /// Suppress lessons below this confidence (default 0.4). Confidence
    /// rides on the `confidence:<f>` tag and is updated by reinforcement.
    #[serde(default = "default_lesson_inject_min_confidence")]
    pub lesson_inject_min_confidence: f32,
    /// Minimum importance for a lesson to be eligible for injection
    /// (default 0.4). Importance is updated atomically by reinforcement.
    #[serde(default = "default_lesson_inject_min_importance")]
    pub lesson_inject_min_importance: f32,
    /// Coarse minimum semantic-recall certainty for a lesson to be injected
    /// (default 0.6, up from a near-useless 0.45). This is only a backstop that
    /// drops outright-irrelevant matches; the relative-to-top band (see
    /// `lessons::RELATIVE_SCORE_BAND`) does the fine relevant-vs-unrelated
    /// separation, because with the production embedding model the two clusters
    /// sit only ~0.1 apart (unrelated ~0.64, relevant ~0.73). Tune per
    /// embedding model — a different model shifts the certainty scale.
    #[serde(default = "default_lesson_inject_min_score")]
    pub lesson_inject_min_score: f32,
    /// Score the turn's BEST lesson must reach before the turn injects anything
    /// (default 0.70). `lesson_inject_min_score` admits individual lessons; this
    /// asks whether the turn has anything relevant at all, because the band is
    /// scale-free and a uniformly irrelevant turn otherwise sits inside its own
    /// band and admits itself. Measured: 29% of turns had a best score under
    /// 0.70 and injected 24% of all lessons between them. `0.0` disables.
    ///
    /// Calibrated on cosine certainty. Setting `lesson_recall_alpha` switches
    /// recall to hybrid relative-score fusion, which normalises the best hit
    /// toward 1.0 — this gate then never fires, so retune it per agent when
    /// enabling hybrid (the same scale trap applies to the two floors above).
    #[serde(default = "default_lesson_inject_min_top_score")]
    pub lesson_inject_min_top_score: f32,
    /// Optional minimum semantic-search certainty (cosine similarity in
    /// `[0, 1]`) applied to every `memory recall`. `None` returns the
    /// top-N regardless of relevance — the legacy behaviour. A value like
    /// `0.65` filters weak matches Weaviate-side so the agent sees only
    /// genuine recall hits. Tune after gathering real-traffic data on
    /// false-positive vs miss rate; ship `None` until that data exists.
    #[serde(default)]
    pub recall_min_certainty: Option<f32>,
    /// Bounded recency boost applied to context-aware memory recall at turn
    /// start (the `## Recalled Memories` block only). A memory whose
    /// `event_time` is `age` hours old has its recall score multiplied by
    /// `1 + recall_recency_boost * 0.5^(age / recall_recency_half_life_hours)`,
    /// capped at 1.0. Small by design: a 5-minute-old same-user memory should
    /// outrank a week-old similarity match without swamping genuinely relevant
    /// old memories. 0.0 disables.
    #[serde(default = "default_recall_recency_boost")]
    pub recall_recency_boost: f32,
    /// Half-life in hours for the recency boost decay (default 6.0).
    #[serde(default = "default_recall_recency_half_life_hours")]
    pub recall_recency_half_life_hours: f32,
    /// Hybrid-search alpha for **lesson** recall/injection (0 = keyword only,
    /// 1 = vector only). `None` (default) keeps the legacy pure-vector
    /// `nearText` path. Set e.g. `0.5` to enable BM25+vector hybrid so
    /// trigger-conditioned lessons surface on keyword overlap even when the
    /// embedding drifts (the "same mistake twice" recall-miss fix). Hot-
    /// reloadable: enable it, then watch recurrence/prevention on the admin
    /// Lessons dashboard; if injection collapses, lower `lesson_inject_min_score`
    /// (hybrid scores use relative fusion, a different scale than cosine certainty).
    #[serde(default)]
    pub lesson_recall_alpha: Option<f32>,
    /// Which agents' memories automatic recall injects into this agent's
    /// prompt. `shared` (default) admits own plus team/global memories;
    /// `isolated` confines recall to the agent's own memories.
    #[serde(default)]
    pub agent_recall_scope: AgentRecallScope,
    /// Enable the periodic lesson decay/prune maintenance pass (default false).
    /// Lessons with no injection/reinforcement/recurrence activity for
    /// `lesson_decay_idle_days` lose `lesson_decay_step` importance each pass,
    /// sinking below the inject floor and eventually pruning at
    /// `lesson_decay_prune_floor` — so stale gotchas stop being injected as the
    /// store grows.
    #[serde(default)]
    pub lesson_decay: bool,
    /// Days without activity before an idle lesson starts decaying (default 30).
    #[serde(default = "default_lesson_decay_idle_days")]
    pub lesson_decay_idle_days: i64,
    /// Importance removed from an idle lesson each decay pass (default 0.05).
    #[serde(default = "default_lesson_decay_step")]
    pub lesson_decay_step: f32,
    /// Importance at or below which a decayed lesson is pruned / soft-deleted
    /// (default 0.1).
    #[serde(default = "default_lesson_decay_prune_floor")]
    pub lesson_decay_prune_floor: f32,
    /// Domain words that mark a stored number as a point-in-time aggregate
    /// ("23 unshipped parcels"), added to the generic defaults the memory audit
    /// already knows ("23 open tickets").
    #[serde(default)]
    pub derived_figures: DerivedFigureVocabulary,
}

/// Domain vocabulary for spotting memories that store a derived figure — a
/// count or metric computed from a query, which goes stale as the data moves.
/// Each list extends the generic defaults; matching is case-insensitive.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DerivedFigureVocabulary {
    /// Aggregate nouns, written singular (`"parcel"`); the plural `s` is
    /// matched too. List irregular plurals separately (`"deliveries"`).
    pub nouns: Vec<String>,
    /// Status words that may sit between the number and the noun
    /// (`"unshipped"` in "23 unshipped parcels").
    pub qualifiers: Vec<String>,
    /// Keys of minted fact lines (`"on-time-rate"` in `on-time-rate: 85`).
    pub fact_keys: Vec<String>,
}

impl MemoryConfig {
    /// Whether memories are kept in vellev.
    pub fn is_vellev(&self) -> bool {
        self.backend == "vellev"
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: default_memory_backend(),
            db_path: default_memory_db_path(),
            embedding_provider: default_embedding_provider(),
            embedding_model: default_embedding_model(),
            embedding_api_url: default_embedding_api_url(),
            embedding_api_key: None,
            passive: false,
            passive_model: None,
            passive_min_turn_tokens: default_passive_min_turn_tokens(),
            passive_prompt: None,
            resume_context: true,
            relationship: false,
            relationship_min_turn_tokens: default_relationship_min_turn_tokens(),
            consolidation: false,
            consolidation_interval_secs: default_consolidation_interval(),
            consolidation_lookback_secs: default_consolidation_lookback(),
            consolidation_model: None,
            consolidation_source: CurationSource::default(),
            consolidation_cc_failover: false,
            lesson_extract: false,
            lesson_extract_model: None,
            lesson_extract_prompt: None,
            lesson_inject: false,
            lesson_inject_limit: default_lesson_inject_limit(),
            lesson_inject_min_confidence: default_lesson_inject_min_confidence(),
            lesson_inject_min_importance: default_lesson_inject_min_importance(),
            lesson_inject_min_score: default_lesson_inject_min_score(),
            lesson_inject_min_top_score: default_lesson_inject_min_top_score(),
            recall_min_certainty: None,
            recall_recency_boost: default_recall_recency_boost(),
            recall_recency_half_life_hours: default_recall_recency_half_life_hours(),
            lesson_recall_alpha: None,
            agent_recall_scope: AgentRecallScope::default(),
            lesson_decay: false,
            lesson_decay_idle_days: default_lesson_decay_idle_days(),
            lesson_decay_step: default_lesson_decay_step(),
            lesson_decay_prune_floor: default_lesson_decay_prune_floor(),
            derived_figures: DerivedFigureVocabulary::default(),
        }
    }
}

fn default_lesson_decay_idle_days() -> i64 {
    30
}

fn default_lesson_decay_step() -> f32 {
    0.05
}

fn default_lesson_decay_prune_floor() -> f32 {
    0.1
}

fn default_passive_min_turn_tokens() -> u32 {
    200
}

fn default_relationship_min_turn_tokens() -> u32 {
    40
}

fn default_consolidation_interval() -> u64 {
    14400 // 4 hours
}

fn default_consolidation_lookback() -> u64 {
    43200 // 12 hours
}

fn default_lesson_inject_limit() -> usize {
    3
}

fn default_lesson_inject_min_confidence() -> f32 {
    0.4
}

fn default_lesson_inject_min_importance() -> f32 {
    0.4
}

fn default_lesson_inject_min_score() -> f32 {
    0.6
}

fn default_lesson_inject_min_top_score() -> f32 {
    0.7
}

fn default_recall_recency_boost() -> f32 {
    0.25
}

fn default_recall_recency_half_life_hours() -> f32 {
    6.0
}

/// What `[memory] backend` may be: `""` reads as `"sqlite"`.
pub const MEMORY_BACKENDS: [&str; 6] = ["sqlite", "", "weaviate", "vellev", SHADOWED, "in_memory"];

fn default_memory_backend() -> String {
    "sqlite".to_string()
}

/// Telemetry / cost / audit store configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// shared Postgres backend (Phase 1B-2 of the migration roadmap).
/// Postgres requires `[postgres] enabled = true` and the workforce-common
/// `postgres` cargo feature; otherwise the SQLite default is used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_telemetry_backend")]
    pub backend: String,
    /// Days a task's telemetry (its rounds and tool calls with it) is kept
    /// after it completed; `0` (the default) keeps it forever. A window
    /// shorter than a report's look back shortens that report: usage tools
    /// read 90 days, cost analytics three months.
    #[serde(default)]
    pub activity_retention_days: u32,
    /// Days a cost record is kept; `0` (the default) keeps them forever.
    /// Costs are the billing ledger: the monthly cost view reads three years
    /// back, and a person's spend is kept as their billing history. A
    /// scheduled job's costs are never pruned, since its budget caps what
    /// the job has spent over its whole life.
    #[serde(default)]
    pub cost_retention_days: u32,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            backend: default_telemetry_backend(),
            activity_retention_days: 0,
            cost_retention_days: 0,
        }
    }
}

fn default_telemetry_backend() -> String {
    "sqlite".to_string()
}

/// Conversation session store configuration.
///
/// Selects between the default JSONL file store and the optional shared
/// Postgres backend (horizontal-scaling inc1b). Postgres requires
/// `[postgres] enabled = true` and the workforce-common `postgres` cargo
/// feature; otherwise the JSONL default is used. Deliberately excluded
/// from `POSTGRES_DEFAULT_STORES`: sessions must be backfilled (via
/// `workforce migrate-sessions`) before cutover, so enabling is explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsConfig {
    /// Backend: `"jsonl"` (default) or `"postgres"`.
    #[serde(default = "default_sessions_backend")]
    pub backend: String,
    /// Inject a digest of the user's other recent sessions with this agent
    /// when a 1:1 DM session opens with no dialogue yet (default true).
    /// Strictly DM-target-gated; never rendered into shared rooms.
    #[serde(default = "default_true")]
    pub digest_enabled: bool,
    /// Max other sessions rendered (default 3).
    #[serde(default = "default_digest_max_sessions")]
    pub digest_max_sessions: usize,
    /// Only sessions active within this window are eligible (default 12).
    #[serde(default = "default_digest_window_hours")]
    pub digest_window_hours: u64,
    /// Total character budget for the digest block (default 1200).
    #[serde(default = "default_digest_max_chars")]
    pub digest_max_chars: usize,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            backend: default_sessions_backend(),
            digest_enabled: true,
            digest_max_sessions: default_digest_max_sessions(),
            digest_window_hours: default_digest_window_hours(),
            digest_max_chars: default_digest_max_chars(),
        }
    }
}

impl SessionsConfig {
    /// The digest knobs bundled as a resolved value, for per-agent override
    /// via the workspace `config.toml` (see `AgentWorkspaceConfig::session_digest`).
    pub fn digest_settings(&self) -> SessionDigestConfig {
        SessionDigestConfig {
            enabled: self.digest_enabled,
            max_sessions: self.digest_max_sessions,
            window_hours: self.digest_window_hours,
            max_chars: self.digest_max_chars,
        }
    }
}

/// Route every `claude -p` subprocess that runs on a **subscription** through
/// the privacy gateway (`crates/privacy-gateway`), which replaces personal
/// data with placeholders before the request leaves the EU and restores them
/// in the reply. Bedrock rounds never route: they already stay in-region.
///
/// Install-wide and read at boot, like `[hub]`: whether this install's
/// subscription traffic leaves the EU scrubbed is not something one agent's
/// workspace should be able to switch off. Enabling it with an incomplete
/// route is a boot error, never a silent pass-through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivacyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The gateway's base URL, e.g. `https://privacy.internal:8787`. Set on the
    /// child as `ANTHROPIC_BASE_URL`; the subscription login is untouched.
    #[serde(default)]
    pub gateway_url: Option<String>,
    /// This install's tenant token, sent as `X-Privacy-Token`. Use
    /// `${PRIVACY_GATEWAY_TOKEN}` to read it from the environment.
    #[serde(default)]
    pub token: Option<String>,
}

/// Typed decisions from a System One model (`[decision]`). Kev (self-hosted)
/// and Jev (TypeSafe's hosted model) speak the same API and are configured
/// the same way; `primary` answers, `fallback` steps in when it fails, and
/// `compare = true` asks both and logs whether they agree without acting on
/// the second. Install-wide, read at boot.
///
/// ```toml
/// [decision]
/// enabled = true
/// primary = "kev"
/// # fallback = "jev"
/// # compare = true
///
/// [[decision.backends]]
/// name = "kev"
/// url = "http://kev:8009"
/// model = "kev-latest"
///
/// [[decision.backends]]
/// name = "jev"
/// url = "https://api.typesafe.ai"
/// model = "jev-1.13.0"
///
/// [decision.branch_fallback]
/// enabled = true
/// min_probability = 0.7
/// shadow = true          # log the pick, do not route on it
/// ```
///
/// A backend takes `max_concurrent` requests at once, and every use has a
/// `budget_ms` (the longest a call may take, queueing included, before its
/// caller moves on without a decision) and a `state_chars` (how much text
/// one of its requests may carry, state and questions together): see
/// [`DecisionLimits`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub backends: Vec<DecisionBackend>,
    /// Name of the backend whose answers are acted on.
    #[serde(default)]
    pub primary: Option<String>,
    /// Name of the backend asked when the primary fails.
    #[serde(default)]
    pub fallback: Option<String>,
    /// Also ask the fallback every time and log agreement.
    #[serde(default)]
    pub compare: bool,
    #[serde(default)]
    pub branch_fallback: BranchFallbackConfig,
    #[serde(default)]
    pub uses: DecisionUses,
    #[serde(default)]
    pub records: DecisionRecordsConfig,
}

/// `[decision.records]`: keep what each decision was asked and what came
/// back, so an install can train and judge a decision model on its own
/// decisions. A record holds the request a person or a step made, so it is
/// read only by admins and the offline export. `enabled` is the install's
/// kill switch; an agent's own `config.toml` can switch its records off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecordsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Days a record is kept; 0 keeps them for good.
    #[serde(default = "default_decision_record_days")]
    pub retention_days: u32,
}

impl Default for DecisionRecordsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: default_decision_record_days(),
        }
    }
}

fn default_decision_record_days() -> u32 {
    90
}

impl DecisionRecordsConfig {
    /// The records config an agent runs with: recorded unless the install
    /// or its own `config.toml` says otherwise.
    pub fn for_agent(&self, agent: Option<&AgentDecisionConfig>) -> Self {
        let own = agent.and_then(|a| a.records).and_then(|r| r.enabled);
        Self {
            enabled: self.enabled && own.unwrap_or(true),
            ..*self
        }
    }
}

/// `[evals]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEvalsConfig {
    /// The agent is a test copy: nobody's own conversations run on it, so an
    /// eval arm may change its settings for the length of a run.
    #[serde(default)]
    pub copy: bool,
}

/// `[decision]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDecisionConfig {
    #[serde(default)]
    pub records: Option<AgentDecisionRecordsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uses: Option<AgentDecisionUses>,
}

/// `[decision.uses]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDecisionUses {
    #[serde(default)]
    pub decide_first: Option<AgentDecideFirstUse>,
}

/// `[decision.uses.decide_first]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDecideFirstUse {
    #[serde(default)]
    pub mode: Option<DecisionUse>,
}

/// `[decision.records]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDecisionRecordsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Which decision-model uses are switched on once `[decision]` is enabled.
/// The branch fallback has its own table above (it carries a shadow flag).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionUses {
    /// Workflow steps that declare `decide = { … }`.
    #[serde(default)]
    pub decision_steps: UseSwitch,
    /// The `decide.ask` agent tool.
    #[serde(default)]
    pub tool: UseSwitch,
    /// The pre-agent scope gate.
    #[serde(default)]
    pub scope_gate: ScopeGateUse,
    /// Classifying a request's use case for a router.
    #[serde(default)]
    pub routing: RoutingUse,
    /// Settling an agent step's outcome before its turn runs.
    #[serde(default)]
    pub decide_first: DecideFirstUse,
}

/// A use that is simply on or off, `decision_steps = true`, or a table when
/// it also sets limits: `decision_steps = { budget_ms = 20000 }`. Anything
/// else — another type, a key the table does not have, a limit out of
/// range — fails to parse, saying which.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum UseSwitch {
    Enabled(bool),
    Limited {
        enabled: bool,
        #[serde(flatten)]
        limits: DecisionLimits,
    },
}

impl<'de> Deserialize<'de> for UseSwitch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            #[serde(default = "default_true")]
            enabled: bool,
            #[serde(default)]
            budget_ms: Option<std::num::NonZeroU64>,
            #[serde(default)]
            state_chars: Option<StateChars>,
        }

        struct Switch;

        impl<'de> serde::de::Visitor<'de> for Switch {
            type Value = UseSwitch;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("true, false, or a table of `enabled`, `budget_ms` and `state_chars`")
            }

            fn visit_bool<E>(self, enabled: bool) -> Result<UseSwitch, E> {
                Ok(UseSwitch::Enabled(enabled))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<UseSwitch, A::Error> {
                let table = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(UseSwitch::Limited {
                    enabled: table.enabled,
                    limits: DecisionLimits {
                        budget_ms: table.budget_ms,
                        state_chars: table.state_chars,
                    },
                })
            }
        }

        deserializer.deserialize_any(Switch)
    }
}

impl Default for UseSwitch {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

impl UseSwitch {
    pub fn enabled(&self) -> bool {
        match self {
            Self::Enabled(enabled) | Self::Limited { enabled, .. } => *enabled,
        }
    }

    pub fn limits(&self) -> DecisionLimits {
        match self {
            Self::Enabled(_) => DecisionLimits::default(),
            Self::Limited { limits, .. } => *limits,
        }
    }
}

/// What one use may spend on a decision. Unset, each use has a default
/// that suits where it sits: routing and the scope gate run before every
/// turn and give up within a fraction of a second, a workflow step waits
/// longer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionLimits {
    /// Milliseconds a call may take on a backend, waiting for one of its
    /// `max_concurrent` slots included. Past it the call fails as busy and
    /// the use takes its non-decision path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_ms: Option<std::num::NonZeroU64>,
    /// Characters of text a whole request may carry, its state and its
    /// questions together (default 1,500, the length the stock models are
    /// trained on). The key keeps the name it had when it bounded the state
    /// alone. Raise it only for a model trained on longer requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_chars: Option<StateChars>,
}

/// A request budget in characters (`state_chars`): at least 200, below
/// which a request cannot hold a question and its context. Smaller values
/// fail to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "usize", into = "usize")]
pub struct StateChars(usize);

impl StateChars {
    pub const MIN: usize = 200;

    pub fn get(self) -> usize {
        self.0
    }
}

impl TryFrom<usize> for StateChars {
    type Error = String;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if value >= Self::MIN {
            Ok(Self(value))
        } else {
            Err(format!(
                "state_chars must be at least {}, got {value}",
                Self::MIN
            ))
        }
    }
}

impl From<StateChars> for usize {
    fn from(chars: StateChars) -> Self {
        chars.0
    }
}

/// Classifying a request's use case (database, code, investigation, action
/// or ambiguous) with the decision model, for a router. `off` by default;
/// `shadow` classifies and logs without anything acting on it; `on` lets a
/// router act on the class. Until a router asks, the only requests
/// classified are the ones the scope gate puts to the decision model, whose
/// request the question rides in. Read at boot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoutingUse {
    #[serde(default)]
    pub mode: DecisionUse,
    /// The picked use case's probability must reach this; below it the
    /// request is `ambiguous`.
    #[serde(default = "default_decision_floor")]
    pub min_probability: Probability,
    #[serde(flatten)]
    pub limits: DecisionLimits,
}

impl Default for RoutingUse {
    fn default() -> Self {
        Self {
            mode: DecisionUse::Off,
            min_probability: default_decision_floor(),
            limits: DecisionLimits::default(),
        }
    }
}

/// The decision model asked before an agent step's turn, for workflow steps
/// that declare `decide_first`: `off` (the default: every such step runs its
/// turn), `shadow` (asked and recorded beside the turn, which always runs)
/// or `on` (a confident answer completes the step and the turn does not
/// run). The install's kill switch, read at boot: an agent's `config.toml`
/// and a step's own `mode` can each hold the use back further, never push
/// it past this.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DecideFirstUse {
    #[serde(default)]
    pub mode: DecisionUse,
    /// What an option's probability must reach to settle a step whose block
    /// sets no `min_probability` of its own.
    #[serde(default = "default_decide_first_floor")]
    pub min_probability: Probability,
    #[serde(flatten)]
    pub limits: DecisionLimits,
}

impl Default for DecideFirstUse {
    fn default() -> Self {
        Self {
            mode: DecisionUse::Off,
            min_probability: default_decide_first_floor(),
            limits: DecisionLimits::default(),
        }
    }
}

impl DecideFirstUse {
    /// How far the use is rolled out for an agent: as far as the install
    /// allows, held back by the agent's own `config.toml`.
    pub fn for_agent(&self, agent: Option<&AgentDecisionConfig>) -> DecisionUse {
        let own = agent
            .and_then(|agent| agent.uses)
            .and_then(|uses| uses.decide_first)
            .and_then(|first| first.mode);
        own.map_or(self.mode, |own| own.min(self.mode))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub validation: ToolValidationConfig,
    #[serde(default)]
    pub loading: ToolLoadingConfig,
    #[serde(default)]
    pub programs: ProgramsConfig,
    #[serde(default)]
    pub compile_gate: CompileGateConfig,
    #[serde(default)]
    pub refactor: RefactorConfig,
}

/// `[embeddings]`: the model memories and queries are embedded with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    /// `none` keeps Weaviate's own vectorizer and the original collection.
    #[serde(default)]
    pub provider: EmbeddingsProvider,
    /// The embedding server (`tei`, `openai`); ignored for `bedrock`.
    #[serde(default)]
    pub url: Option<String>,
    /// The model: a Bedrock model id, or the served model's name.
    #[serde(default)]
    pub model: Option<String>,
    /// AWS region for `bedrock` (defaults to the hub's Bedrock region).
    #[serde(default)]
    pub region: Option<String>,
    /// Text put before every query, as the model was trained (e.g. bge
    /// "Represent this sentence for searching relevant passages: ", nomic
    /// "search_query: ", Qwen3-Embedding "Instruct: <task>\nQuery: ").
    #[serde(default)]
    pub query_prefix: String,
    /// Text put before every memory embedded (e.g. nomic "search_document: ").
    #[serde(default)]
    pub document_prefix: String,
    /// Vector size to request where the model offers a choice.
    #[serde(default)]
    pub dimensions: Option<u32>,
    /// The collection the hub's vectors live in; by default named after the
    /// model, so changing model starts a fresh collection (and sync) rather
    /// than mixing two models' vectors.
    #[serde(default)]
    pub collection: Option<String>,
    /// Keyword weight in hybrid recall: 0 = keywords only, 1 = vectors only.
    #[serde(default = "default_embeddings_alpha")]
    pub alpha: f32,
    /// A text-embeddings-inference reranker that reorders recall candidates.
    #[serde(default)]
    pub rerank_url: Option<String>,
    /// Candidates fetched for reranking.
    #[serde(default = "default_rerank_candidates")]
    pub rerank_candidates: usize,
    /// `[[certainty, reported], ...]` ascending: maps this model's certainty
    /// onto the scale the recall, lesson and near-duplicate thresholds were
    /// tuned on (MiniLM), linearly between knots. Ordering is unchanged.
    /// Empty reports raw certainty.
    #[serde(default)]
    pub score_map: Vec<[f32; 2]>,
    /// Whether the store memories switched from (the collection Weaviate
    /// or vellev embeds for itself, `WorkforceMemory`) is kept beside the
    /// one the hub embeds for: forgetting and erasure reaching it, new
    /// memories caught up from it for half an hour after each start and
    /// carried back to it after, so that switching back loses nothing.
    /// `false` leaves it alone entirely, read and written by nothing of
    /// the hub's: what promoting vellev needs (DW-676), since a previous
    /// store nothing erases from would bring an erased memory back.
    #[serde(default = "default_true")]
    pub keep_previous: bool,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingsProvider::None,
            url: None,
            model: None,
            region: None,
            query_prefix: String::new(),
            document_prefix: String::new(),
            dimensions: None,
            collection: None,
            alpha: default_embeddings_alpha(),
            rerank_url: None,
            rerank_candidates: default_rerank_candidates(),
            score_map: Vec::new(),
            keep_previous: true,
        }
    }
}

impl EmbeddingsConfig {
    pub fn enabled(&self) -> bool {
        self.provider != EmbeddingsProvider::None
    }

    /// The model `model` names, or the provider's default one.
    pub fn model_name(&self) -> &str {
        match (self.model.as_deref(), self.provider) {
            (Some(m), _) if !m.is_empty() => m,
            (_, EmbeddingsProvider::Bedrock) => "amazon.titan-embed-text-v2:0",
            _ => "",
        }
    }

    /// `collection`, or `WorkforceMemory_<model>_<dimensions>` with the
    /// model's punctuation as underscores.
    pub fn collection_name(&self) -> String {
        if let Some(c) = self.collection.as_deref().filter(|c| !c.is_empty()) {
            return c.to_string();
        }
        let model: String = self
            .model_name()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        let mut name = format!("WorkforceMemory_{model}");
        if let Some(d) = self.dimensions {
            name.push_str(&format!("_{d}"));
        }
        name
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingsProvider {
    #[default]
    None,
    /// A text-embeddings-inference server (`url`).
    Tei,
    /// An OpenAI-compatible `/v1/embeddings` endpoint (`url`; key from the
    /// global vault at `embeddings/openai/api_key`, else `EMBEDDINGS_API_KEY`).
    Openai,
    /// Amazon Bedrock (`model`, e.g. `amazon.titan-embed-text-v2:0`).
    Bedrock,
}

fn default_embeddings_alpha() -> f32 {
    0.75
}

fn default_rerank_candidates() -> usize {
    30
}

/// `[tools.programs]`: Sift programs an agent writes to answer a data question
/// in one run instead of many tool rounds.
/// Every field may also be set in an agent's own `config.toml`
/// ([`AgentProgramsConfig`]), which overrides this one field by field and
/// takes effect on the agent's next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramsConfig {
    #[serde(default)]
    pub mode: ProgramsMode,
    /// Under `on`, when not empty, only these agents run programs, unless an
    /// agent's own `config.toml` says otherwise.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Under `on`, agents that run no programs, unless an agent's own
    /// `config.toml` says otherwise.
    #[serde(default)]
    pub except: Vec<String>,
    /// The tools a program may call, by name or namespace: only read-only
    /// tools an admin vouches for. A native SQL tool reads only if its
    /// database account cannot write, and listing an action-dispatch tool
    /// allows every one of its actions.
    #[serde(default)]
    pub read_tools: Vec<String>,
    /// The tool a program's `sql` block runs through.
    #[serde(default = "default_program_sql_tool")]
    pub sql_tool: String,
    #[serde(default = "default_program_max_calls")]
    pub max_calls: u32,
    #[serde(default = "default_program_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_program_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_program_max_steps")]
    pub max_steps: u64,
    #[serde(default = "default_program_max_return_bytes")]
    pub max_return_bytes: usize,
    /// Rows a program's `sql` block asks for. The rows reach the program,
    /// never the model, so this can be far above an inline result; the SQL
    /// tool's own ceiling still applies.
    #[serde(default = "default_program_max_sql_rows")]
    pub max_sql_rows: u64,
    /// After this many direct calls in one turn to tools a program could
    /// make, the next tool result carries a one-line reminder that
    /// `program_run` can do the rest in one go. `0` never reminds.
    #[serde(default = "default_program_nudge_after")]
    pub nudge_after: u32,
    /// Every turn's request carries one line saying when to answer with a
    /// program. A model reads guidance in the request as part of the ask;
    /// the same words inside a tool result read as data.
    #[serde(default = "default_true")]
    pub hint: bool,
}

impl Default for ProgramsConfig {
    fn default() -> Self {
        Self {
            mode: ProgramsMode::default(),
            agents: Vec::new(),
            except: Vec::new(),
            read_tools: Vec::new(),
            sql_tool: default_program_sql_tool(),
            max_calls: default_program_max_calls(),
            max_concurrency: default_program_max_concurrency(),
            timeout_secs: default_program_timeout_secs(),
            max_steps: default_program_max_steps(),
            max_return_bytes: default_program_max_return_bytes(),
            max_sql_rows: default_program_max_sql_rows(),
            nudge_after: default_program_nudge_after(),
            hint: true,
        }
    }
}

impl ProgramsConfig {
    /// Whether `agent_id` runs programs, given what its own `config.toml`
    /// says (`[tools.programs] enabled`), if anything.
    pub fn enabled_for(&self, agent_id: &str, agent_setting: Option<bool>) -> bool {
        let listed = |agents: &[String]| agents.iter().any(|agent| agent == agent_id);
        match self.mode {
            ProgramsMode::Off => false,
            ProgramsMode::OptIn => agent_setting == Some(true),
            ProgramsMode::On => agent_setting.unwrap_or_else(|| {
                !listed(&self.except) && (self.agents.is_empty() || listed(&self.agents))
            }),
        }
    }

    /// The programs config `agent_id` runs with, `agent` (its own
    /// `config.toml`) overriding this one field by field; `None` when it
    /// runs no programs.
    pub fn for_agent(&self, agent_id: &str, agent: Option<&AgentProgramsConfig>) -> Option<Self> {
        if !self.enabled_for(agent_id, agent.and_then(|a| a.enabled)) {
            return None;
        }
        let Some(agent) = agent else {
            return Some(self.clone());
        };
        Some(Self {
            read_tools: agent
                .read_tools
                .clone()
                .unwrap_or_else(|| self.read_tools.clone()),
            sql_tool: agent
                .sql_tool
                .clone()
                .unwrap_or_else(|| self.sql_tool.clone()),
            max_calls: agent.max_calls.unwrap_or(self.max_calls),
            max_concurrency: agent.max_concurrency.unwrap_or(self.max_concurrency),
            timeout_secs: agent.timeout_secs.unwrap_or(self.timeout_secs),
            max_steps: agent.max_steps.unwrap_or(self.max_steps),
            max_return_bytes: agent.max_return_bytes.unwrap_or(self.max_return_bytes),
            max_sql_rows: agent.max_sql_rows.unwrap_or(self.max_sql_rows),
            nudge_after: agent.nudge_after.unwrap_or(self.nudge_after),
            hint: agent.hint.unwrap_or(self.hint),
            ..self.clone()
        })
    }
}

/// `[tools.compile_gate]`: after a turn that edited source files in a repo,
/// the repo's build check runs before the answer goes out, and its errors go
/// back to the agent to fix. Every field may
/// also be set in an agent's own `config.toml` ([`AgentCompileGateConfig`]),
/// which overrides this one field by field and takes effect on its next turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompileGateConfig {
    /// `off` everywhere, `opt_in` (default: agents whose own config enables
    /// it) or `on` (every agent unless its own config says otherwise).
    #[serde(default)]
    pub mode: ProgramsMode,
    /// Rounds the agent is given to fix a failing build before the turn
    /// ends with the failure reported in its answer.
    #[serde(default = "default_compile_gate_max_fix_rounds")]
    pub max_fix_rounds: u32,
    /// Wall clock for one check command. A check that runs out is reported
    /// as unverified, never as passing.
    #[serde(default = "default_compile_gate_timeout_secs")]
    pub timeout_secs: u64,
    /// Checks declared per repo. A repo with any declared check runs only
    /// those; any other repo falls back to detection from its manifests.
    #[serde(default)]
    pub checks: Vec<DeclaredCheck>,
}

impl Default for CompileGateConfig {
    fn default() -> Self {
        Self {
            mode: ProgramsMode::OptIn,
            max_fix_rounds: default_compile_gate_max_fix_rounds(),
            timeout_secs: default_compile_gate_timeout_secs(),
            checks: Vec::new(),
        }
    }
}

impl CompileGateConfig {
    /// The compile gate `agent` (its own `config.toml`) runs with, over this
    /// one field by field; `None` when it has none.
    pub fn for_agent(&self, agent: Option<&AgentCompileGateConfig>) -> Option<Self> {
        let enabled = match self.mode {
            ProgramsMode::Off => false,
            ProgramsMode::OptIn => agent.and_then(|a| a.enabled) == Some(true),
            ProgramsMode::On => agent.and_then(|a| a.enabled).unwrap_or(true),
        };
        if !enabled {
            return None;
        }
        let Some(agent) = agent else {
            return Some(self.clone());
        };
        Some(Self {
            max_fix_rounds: agent.max_fix_rounds.unwrap_or(self.max_fix_rounds),
            timeout_secs: agent.timeout_secs.unwrap_or(self.timeout_secs),
            checks: agent.checks.clone().unwrap_or_else(|| self.checks.clone()),
            ..self.clone()
        })
    }
}

/// One `[[tools.compile_gate.checks]]` entry: the command that checks the
/// project at `path` in `repo` builds. The command is run as written, never
/// through a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredCheck {
    /// The repo as `owner/name`, the way `workspace/repos/` lays it out.
    pub repo: String,
    /// The project directory inside the repo, relative to its root; empty
    /// for the root. Edits under it are checked by this command, run there.
    #[serde(default)]
    pub path: String,
    /// The program and its arguments, e.g. `["cargo", "check", "--workspace"]`.
    pub command: Vec<String>,
}

/// `[tools.compile_gate]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentCompileGateConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_fix_rounds: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub checks: Option<Vec<DeclaredCheck>>,
}

/// `[tools.refactor]`: `code.refactor`, structural code edits
///. Replacing a definition by name needs
/// nothing running; renames, diagnostics and navigation are answered by a
/// language server the hub starts per project through the executor. Whether
/// an agent has it is read each turn from its own `config.toml`
/// ([`AgentRefactorConfig`]); the limits on servers are the hub's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefactorConfig {
    /// `off` everywhere, `opt_in` (default: agents whose own config enables
    /// it) or `on` (every agent unless its own config says otherwise).
    #[serde(default)]
    pub mode: ProgramsMode,
    /// Whether the actions a language server answers are offered at all.
    #[serde(default = "default_true")]
    pub language_servers: bool,
    /// Language servers running in one hub process at once, across its
    /// agents. With `max_memory_mb` this is the memory the feature may take:
    /// the defaults keep it to 2 GiB a replica.
    #[serde(default = "default_refactor_max_servers")]
    pub max_servers: usize,
    /// A server unused this long is shut down.
    #[serde(default = "default_refactor_idle_secs")]
    pub idle_secs: u64,
    /// A server is stopped this long after it started, used or not.
    #[serde(default = "default_refactor_max_lifetime_secs")]
    pub max_lifetime_secs: u64,
    /// Resident memory a server and its children may use before they are
    /// stopped. A server is not started while the host has less than this
    /// and a gibibyte of headroom available.
    #[serde(default = "default_refactor_max_memory_mb")]
    pub max_memory_mb: u64,
    /// How long a call waits for a new server to load its project.
    #[serde(default = "default_refactor_startup_timeout_secs")]
    pub startup_timeout_secs: u64,
    /// How long one request to a running server may take.
    #[serde(default = "default_refactor_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// The command that starts the Rust language server.
    #[serde(default = "default_refactor_rust_analyzer")]
    pub rust_analyzer: Vec<String>,
    /// The command that starts the TypeScript and JavaScript language server.
    #[serde(default = "default_refactor_typescript")]
    pub typescript: Vec<String>,
}

impl Default for RefactorConfig {
    fn default() -> Self {
        Self {
            mode: ProgramsMode::OptIn,
            language_servers: true,
            max_servers: default_refactor_max_servers(),
            idle_secs: default_refactor_idle_secs(),
            max_lifetime_secs: default_refactor_max_lifetime_secs(),
            max_memory_mb: default_refactor_max_memory_mb(),
            startup_timeout_secs: default_refactor_startup_timeout_secs(),
            request_timeout_secs: default_refactor_request_timeout_secs(),
            rust_analyzer: default_refactor_rust_analyzer(),
            typescript: default_refactor_typescript(),
        }
    }
}

impl RefactorConfig {
    /// What `code.refactor` offers `agent` (its own `config.toml`); `None`
    /// when it has no structural editing. An agent may turn the language
    /// servers off for itself, never on against the install.
    pub fn for_agent(&self, agent: Option<&AgentRefactorConfig>) -> Option<Self> {
        let enabled = match self.mode {
            ProgramsMode::Off => false,
            ProgramsMode::OptIn => agent.and_then(|a| a.enabled) == Some(true),
            ProgramsMode::On => agent.and_then(|a| a.enabled).unwrap_or(true),
        };
        enabled.then(|| Self {
            language_servers: self.language_servers
                && agent.and_then(|a| a.language_servers).unwrap_or(true),
            ..self.clone()
        })
    }
}

/// `[tools.refactor]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRefactorConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub language_servers: Option<bool>,
}

fn default_refactor_max_servers() -> usize {
    1
}

fn default_refactor_idle_secs() -> u64 {
    900
}

fn default_refactor_max_lifetime_secs() -> u64 {
    4 * 3600
}

fn default_refactor_max_memory_mb() -> u64 {
    2048
}

fn default_refactor_startup_timeout_secs() -> u64 {
    300
}

fn default_refactor_request_timeout_secs() -> u64 {
    60
}

fn default_refactor_rust_analyzer() -> Vec<String> {
    vec!["rust-analyzer".into()]
}

fn default_refactor_typescript() -> Vec<String> {
    vec!["typescript-language-server".into(), "--stdio".into()]
}

fn default_compile_gate_max_fix_rounds() -> u32 {
    2
}

fn default_compile_gate_timeout_secs() -> u64 {
    600
}

/// `[know_how]`: agent-scoped know-how learned from how the agent is used
///. Entries hold only names checked against
/// something real (tables, columns, a query shape with every value masked),
/// and are shared with an agent's users once `min_users` different people
/// produced them. Every field may also be set in an agent's own
/// `config.toml` ([`AgentKnowHowConfig`]), read each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowHowConfig {
    /// `off` everywhere, `opt_in` (default: agents whose own config enables
    /// it) or `on` (every agent unless its own config says otherwise).
    #[serde(default)]
    pub mode: ProgramsMode,
    /// Record entries from finished turns.
    #[serde(default = "default_true")]
    pub extract: bool,
    /// `off`, `shadow` (match and record what would have been shown, show
    /// nothing) or `on`.
    #[serde(default)]
    pub inject: KnowHowInject,
    /// Different people who must have produced an entry before anyone else
    /// is shown it. Admin additions are shown at once. An agent's own config
    /// may raise this, not lower it.
    #[serde(default = "default_know_how_min_users")]
    pub min_users: u32,
    /// Entries shown with one request at most.
    #[serde(default = "default_know_how_max_entries")]
    pub max_entries: usize,
    /// Size of the know-how shown with one request, in tokens (~4 chars each).
    #[serde(default = "default_know_how_max_tokens")]
    pub max_tokens: usize,
    /// Similarity (certainty, 0..1) a request needs with an entry's learned
    /// intents to be shown it.
    #[serde(default = "default_know_how_min_score")]
    pub min_score: f32,
    /// The kinds of entry a request is matched against (every kind unless
    /// set). An agent's own config may leave kinds out, never add one the
    /// install leaves out.
    #[serde(default = "KnowHowKind::all")]
    pub kinds: Vec<KnowHowKind>,
    /// The agent whose entries a test copy reads, read only: its own
    /// `[know_how] source_agent`, kept by [`Self::for_workspace`] only for
    /// an agent that is one (`[evals] copy = true`). Never the install's.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
}

impl Default for KnowHowConfig {
    fn default() -> Self {
        Self {
            mode: ProgramsMode::OptIn,
            extract: true,
            inject: KnowHowInject::default(),
            min_users: default_know_how_min_users(),
            max_entries: default_know_how_max_entries(),
            max_tokens: default_know_how_max_tokens(),
            min_score: default_know_how_min_score(),
            kinds: KnowHowKind::all(),
            source_agent: None,
        }
    }
}

impl KnowHowConfig {
    /// The know-how config `agent_id` runs with, its own `config.toml`
    /// overriding this field by field; `None` when it has no know-how.
    pub fn for_agent(&self, agent: Option<&AgentKnowHowConfig>) -> Option<Self> {
        let enabled = match self.mode {
            ProgramsMode::Off => false,
            ProgramsMode::OptIn => agent.and_then(|a| a.enabled) == Some(true),
            ProgramsMode::On => agent.and_then(|a| a.enabled).unwrap_or(true),
        };
        if !enabled {
            return None;
        }
        let Some(agent) = agent else {
            return Some(self.clone());
        };
        Some(Self {
            extract: agent.extract.unwrap_or(self.extract),
            inject: agent.inject.unwrap_or(self.inject),
            // An agent may ask for more confirmations, never fewer than the
            // install's: whoever can edit one agent's config cannot turn
            // separation off.
            min_users: agent
                .min_users
                .unwrap_or(self.min_users)
                .max(self.min_users)
                .max(1),
            max_entries: agent.max_entries.unwrap_or(self.max_entries),
            max_tokens: agent.max_tokens.unwrap_or(self.max_tokens),
            min_score: agent.min_score.unwrap_or(self.min_score),
            kinds: match &agent.kinds {
                Some(own) => own
                    .iter()
                    .copied()
                    .filter(|k| self.kinds.contains(k))
                    .collect(),
                None => self.kinds.clone(),
            },
            ..self.clone()
        })
    }

    /// [`Self::for_agent`] for an agent whose `config.toml` is `own`, with
    /// the agent a test copy reads its entries from.
    pub fn for_workspace(&self, own: Option<&AgentWorkspaceConfig>) -> Option<Self> {
        Some(Self {
            source_agent: own
                .and_then(AgentWorkspaceConfig::know_how_source)
                .map(str::to_string),
            ..self.for_agent(own.and_then(|own| own.know_how.as_ref()))?
        })
    }

    /// Whose entries `agent` reads: its own, or a test copy's source's.
    pub fn source<'a>(&'a self, agent: &'a str) -> &'a str {
        self.source_agent.as_deref().unwrap_or(agent)
    }

    /// Whether the agent only reads another agent's know-how: then nothing
    /// a turn on it does is recorded anywhere.
    pub fn read_only(&self) -> bool {
        self.source_agent.is_some()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowHowInject {
    Off,
    #[default]
    Shadow,
    On,
}

/// What a know-how entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowHowKind {
    /// Which tables (and columns) of which database answer a kind of request.
    DataLocation,
    /// A query that worked, every value masked.
    QueryShape,
    /// A table or column a query named that does not exist, and the real one
    /// its successful retry used.
    SchemaCorrection,
    /// A file (and the symbol in it) a code search hit that was then read.
    CodeLocation,
}

impl KnowHowKind {
    pub const ALL: [Self; 4] = [
        Self::DataLocation,
        Self::QueryShape,
        Self::SchemaCorrection,
        Self::CodeLocation,
    ];

    pub fn all() -> Vec<Self> {
        Self::ALL.to_vec()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DataLocation => "data_location",
            Self::QueryShape => "query_shape",
            Self::SchemaCorrection => "schema_correction",
            Self::CodeLocation => "code_location",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// `[know_how]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentKnowHowConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub extract: Option<bool>,
    #[serde(default)]
    pub inject: Option<KnowHowInject>,
    #[serde(default)]
    pub min_users: Option<u32>,
    #[serde(default)]
    pub max_entries: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub min_score: Option<f32>,
    #[serde(default)]
    pub kinds: Option<Vec<KnowHowKind>>,
    /// On a test copy (`[evals] copy = true`), the agent whose know-how it
    /// reads in place of its own (empty) one, and never writes to. Ignored
    /// on any other agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
}

fn default_know_how_min_users() -> u32 {
    2
}

fn default_know_how_max_entries() -> usize {
    5
}

fn default_know_how_max_tokens() -> usize {
    800
}

fn default_know_how_min_score() -> f32 {
    0.75
}

/// `[research]`: how `investigate` holds its findings to account
///. Only the kill switch lives here; the
/// knobs are per agent ([`AgentResearchConfig`]), read each call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchConfig {
    /// Whether any agent may run adversarial research. `false` refuses it
    /// everywhere, whatever an agent's config or a call asks for.
    #[serde(default = "default_true")]
    pub allow_adversarial: bool,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            allow_adversarial: true,
        }
    }
}

impl ResearchConfig {
    /// The research policy an agent runs with, its own `config.toml` over
    /// the defaults.
    pub fn for_agent(&self, agent: Option<&AgentResearchConfig>) -> ResearchPolicy {
        ResearchPolicy {
            adversarial_allowed: self.allow_adversarial,
            adversarial_default: agent.and_then(|a| a.adversarial).unwrap_or(false),
            low_confidence_below: agent
                .and_then(|a| a.low_confidence_below)
                .unwrap_or(DEFAULT_LOW_CONFIDENCE_BELOW)
                .min(100),
        }
    }
}

/// A finding whose confidence (0–100) is below this is reported as low
/// confidence rather than stated as fact.
pub const DEFAULT_LOW_CONFIDENCE_BELOW: u8 = 60;

/// `[research]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResearchConfig {
    /// Run `investigate` adversarially when a call does not say. Roughly
    /// doubles the workers a call costs, so off unless set.
    #[serde(default)]
    pub adversarial: Option<bool>,
    /// Confidence (0–100) below which a finding is reported as low confidence.
    #[serde(default)]
    pub low_confidence_below: Option<u8>,
}

/// [`ResearchConfig`] resolved for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResearchPolicy {
    pub adversarial_allowed: bool,
    pub adversarial_default: bool,
    pub low_confidence_below: u8,
}

impl ResearchPolicy {
    /// Whether a call runs adversarially: what it `requested`, else the
    /// agent's default — and never when the install forbids it.
    pub fn adversarial(&self, requested: Option<bool>) -> bool {
        self.adversarial_allowed && requested.unwrap_or(self.adversarial_default)
    }
}

/// `[handoff]`: whether a verifier checks what a workflow step or sub-agent
/// hands on. Only the kill switch lives
/// here; the knobs are per agent ([`AgentHandoffConfig`]), read each time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffConfig {
    /// Whether any hand-over may be verified. `false` turns verification off
    /// everywhere, whatever an agent's config, a step or a call asks for.
    #[serde(default = "default_true")]
    pub allow_verify: bool,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self { allow_verify: true }
    }
}

impl HandoffConfig {
    /// The hand-over policy an agent runs with, its own `config.toml` over
    /// the defaults.
    pub fn for_agent(&self, agent: Option<&AgentHandoffConfig>) -> HandoffPolicy {
        HandoffPolicy {
            verify_allowed: self.allow_verify,
            verify_default: agent.and_then(|a| a.verify).unwrap_or(false),
            max_corrections: agent
                .and_then(|a| a.max_corrections)
                .unwrap_or(DEFAULT_HANDOFF_CORRECTIONS)
                .min(MAX_HANDOFF_CORRECTIONS),
            model: agent.and_then(|a| a.model.clone()),
        }
    }
}

/// Times a verifier sends a producer back before the hand-over fails, unless
/// the agent or the hand-over says otherwise.
pub const DEFAULT_HANDOFF_CORRECTIONS: u32 = 1;

/// The most correction rounds any hand-over gets.
pub const MAX_HANDOFF_CORRECTIONS: u32 = 3;

/// `[handoff]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHandoffConfig {
    /// Verify a hand-over that declares a package when the step or call does
    /// not say. One extra cheap call each, so off unless set.
    #[serde(default)]
    pub verify: Option<bool>,
    /// Correction rounds a rejected hand-over gets (0–3).
    #[serde(default)]
    pub max_corrections: Option<u32>,
    /// The model the verifier runs on; the cheap tier when unset.
    #[serde(default)]
    pub model: Option<String>,
}

/// [`HandoffConfig`] resolved for one agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffPolicy {
    pub verify_allowed: bool,
    pub verify_default: bool,
    pub max_corrections: u32,
    pub model: Option<String>,
}

impl HandoffPolicy {
    /// Whether a hand-over is verified: what it `requested`, else the
    /// agent's default — and never when the install forbids it.
    pub fn verify(&self, requested: Option<bool>) -> bool {
        self.verify_allowed && requested.unwrap_or(self.verify_default)
    }

    /// The correction rounds a hand-over gets: what it `requested`, else the
    /// agent's own, never past [`MAX_HANDOFF_CORRECTIONS`].
    pub fn corrections(&self, requested: Option<u32>) -> u32 {
        requested
            .unwrap_or(self.max_corrections)
            .min(MAX_HANDOFF_CORRECTIONS)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgramsMode {
    /// No agent runs programs, whatever its own config says.
    Off,
    /// Only agents whose own `config.toml` sets `[tools.programs] enabled = true`.
    #[default]
    OptIn,
    /// The agents `agents` and `except` pick, each overridable by the agent's
    /// own `config.toml`.
    On,
}

/// `[tools]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentToolsConfig {
    #[serde(default)]
    pub programs: Option<AgentProgramsConfig>,
    #[serde(default)]
    pub compile_gate: Option<AgentCompileGateConfig>,
    #[serde(default)]
    pub refactor: Option<AgentRefactorConfig>,
}

/// `[tools.programs]` in an agent's `config.toml`: whether this agent runs
/// Sift programs and how, each field overriding the install's
/// `[tools.programs]` (an install `mode = "off"` still turns them all off).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentProgramsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub read_tools: Option<Vec<String>>,
    #[serde(default)]
    pub sql_tool: Option<String>,
    #[serde(default)]
    pub max_calls: Option<u32>,
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_steps: Option<u64>,
    #[serde(default)]
    pub max_return_bytes: Option<usize>,
    #[serde(default)]
    pub max_sql_rows: Option<u64>,
    #[serde(default)]
    pub nudge_after: Option<u32>,
    #[serde(default)]
    pub hint: Option<bool>,
}

fn default_program_nudge_after() -> u32 {
    3
}

fn default_program_max_sql_rows() -> u64 {
    50_000
}

fn default_program_sql_tool() -> String {
    "metabase.dataset_query_native".to_string()
}

fn default_program_max_calls() -> u32 {
    60
}

fn default_program_max_concurrency() -> usize {
    4
}

fn default_program_timeout_secs() -> u64 {
    120
}

fn default_program_max_steps() -> u64 {
    200_000
}

fn default_program_max_return_bytes() -> usize {
    60_000
}

/// `[tools.loading]`: offering an agent the tools it uses in full and every
/// other tool as a catalogue line it loads the full definition from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLoadingConfig {
    #[serde(default)]
    pub mode: ToolLoadingMode,
    /// When not empty, only these agents load on demand (or shadow it).
    #[serde(default)]
    pub agents: Vec<String>,
    /// Agents that never load on demand, whatever `agents` says.
    #[serde(default)]
    pub except: Vec<String>,
    /// The agent's usage is read over this many days.
    #[serde(default = "default_loading_window_days")]
    pub window_days: u32,
    /// The largest share of an agent's turns allowed to need a tool loaded
    /// mid-turn; its resident tools are tuned daily to stay within it.
    #[serde(default = "default_loading_budget")]
    pub load_budget: f64,
    /// Per-agent overrides of `load_budget`.
    #[serde(default)]
    pub budgets: std::collections::BTreeMap<String, f64>,
    /// A tool a conversation used or loaded stays resident for this many of
    /// its user turns.
    #[serde(default = "default_loading_keep_turns")]
    pub keep_turns: usize,
    /// Tool names or namespaces always resident, besides the runtime's own.
    #[serde(default)]
    pub always: Vec<String>,
}

impl Default for ToolLoadingConfig {
    fn default() -> Self {
        Self {
            mode: ToolLoadingMode::default(),
            agents: Vec::new(),
            except: Vec::new(),
            window_days: default_loading_window_days(),
            load_budget: default_loading_budget(),
            budgets: std::collections::BTreeMap::new(),
            keep_turns: default_loading_keep_turns(),
            always: Vec::new(),
        }
    }
}

impl ToolLoadingConfig {
    /// The mode that applies to `agent_id`.
    pub fn mode_for(&self, agent_id: &str) -> ToolLoadingMode {
        let listed = |agents: &[String]| agents.iter().any(|agent| agent == agent_id);
        match !listed(&self.except) && (self.agents.is_empty() || listed(&self.agents)) {
            true => self.mode,
            false => ToolLoadingMode::Off,
        }
    }

    /// The load budget that applies to `agent_id`.
    pub fn budget_for(&self, agent_id: &str) -> f64 {
        self.budgets
            .get(agent_id)
            .copied()
            .unwrap_or(self.load_budget)
    }
}

fn default_loading_window_days() -> u32 {
    14
}

fn default_loading_budget() -> f64 {
    0.10
}

fn default_loading_keep_turns() -> usize {
    10
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolLoadingMode {
    /// Every tool is offered in full, except where a small context window or
    /// a very large MCP server forces loading.
    Off,
    /// The loading plan is worked out and logged, with every call that would
    /// have needed a load; requests are unchanged.
    Shadow,
    #[default]
    On,
}

/// `[tools.validation]`: checking a tool call's arguments against the tool's
/// input schema before it runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolValidationConfig {
    #[serde(default)]
    pub mode: ArgumentValidation,
}

/// What a call with arguments its schema rejects meets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgumentValidation {
    Off,
    /// Checked and logged; the call runs as it would without the check.
    #[default]
    Shadow,
    /// The call does not run; the model is told what to fix.
    Enforce,
}

/// How far a decision-model use is rolled out, least first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionUse {
    #[default]
    Off,
    /// The existing path decides; the decision model is asked alongside and
    /// its agreement logged.
    Shadow,
    On,
}

/// The decision model behind the pre-agent scope gate: `off` (the LLM
/// classifies), `shadow` (the LLM classifies, the decision model is asked
/// alongside and agreement is logged) or `on` (the decision model classifies,
/// no model turn). A scope's own `classifier` overrides the mode. Read at
/// boot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScopeGateUse {
    #[serde(default)]
    pub mode: DecisionUse,
    /// How sure the model must be that a message is off-topic to reject it,
    /// for scopes that set no `min_probability` of their own.
    #[serde(default = "default_decision_floor")]
    pub min_probability: Probability,
    #[serde(flatten)]
    pub limits: DecisionLimits,
}

impl Default for ScopeGateUse {
    fn default() -> Self {
        Self {
            mode: DecisionUse::Off,
            min_probability: default_decision_floor(),
            limits: DecisionLimits::default(),
        }
    }
}

/// A probability floor a decision must reach: more than 0 (a floor of 0 would
/// act on any answer) and at most 1. Out-of-range values fail to parse, so
/// no config path — file, overlay or override — can carry one.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Probability(f64);

impl Probability {
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Probability {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value > 0.0 && value <= 1.0 {
            Ok(Self(value))
        } else {
            Err(format!(
                "a probability floor must be more than 0 and at most 1, got {value}"
            ))
        }
    }
}

impl From<Probability> for f64 {
    fn from(p: Probability) -> Self {
        p.0
    }
}

fn default_decision_floor() -> Probability {
    Probability(0.7)
}

/// Settling a step skips the turn that would have caught a wrong call, so
/// the stock floor is higher than a use with a path beside it needs.
fn default_decide_first_floor() -> Probability {
    Probability(0.9)
}

/// One System One server. Its key is read from the layered vault at
/// `decision/<name>/api_key` on every call, for the person and agent the
/// call is for; `api_key` here is only the last resort when no vault tier
/// holds one.
#[derive(Clone, Serialize, Deserialize)]
pub struct DecisionBackend {
    pub name: String,
    /// Base URL; `/v1/systemone` is appended.
    pub url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    /// Whether a caller with no key at any level is kept away from this
    /// backend (it is skipped for that call). Unset, it is true for a
    /// backend people bring their own key to (Jev) and for one at a public
    /// address, and false for one on a private network, so a keyless
    /// internal Kev keeps working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_key: Option<bool>,
    #[serde(default = "default_decision_model")]
    pub model: String,
    /// The longest any one request may take, whatever its use's budget.
    #[serde(default = "default_decision_timeout_secs")]
    pub timeout_secs: u64,
    /// Requests the backend may be working on at once for this hub process;
    /// further calls wait their turn within their budget. The cap is per
    /// process, so the backend can see this many from every hub replica:
    /// set it to what the hardware serves well divided by the replica
    /// count. One L4 GPU serving Kev-4B holds p50 at 1.4 s with 8 long
    /// states in flight and slips to 3–4 s at 16, past the scope gate's
    /// budget, so the default of 4 keeps two replicas at that 8; a single
    /// hub on such a GPU can take 8, a CPU-only backend 1 or 2.
    #[serde(default = "default_decision_max_concurrent")]
    pub max_concurrent: usize,
}

/// Choosing a workflow branch when an agent step ends without a usable
/// `OUTCOME:` line. Shadow by default: the pick is logged and stamped on the
/// step's provenance, and the run takes today's route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchFallbackConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The chosen label's probability must reach this to be used.
    #[serde(default = "default_decision_floor")]
    pub min_probability: Probability,
    #[serde(default = "default_true")]
    pub shadow: bool,
    #[serde(flatten)]
    pub limits: DecisionLimits,
}

impl Default for BranchFallbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_probability: default_decision_floor(),
            shadow: true,
            limits: DecisionLimits::default(),
        }
    }
}

impl std::fmt::Debug for DecisionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionBackend")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("requires_key", &self.requires_key)
            .field("model", &self.model)
            .field("timeout_secs", &self.timeout_secs)
            .field("max_concurrent", &self.max_concurrent)
            .finish()
    }
}

fn default_decision_model() -> String {
    "kev-latest".to_string()
}

fn default_decision_timeout_secs() -> u64 {
    20
}

fn default_decision_max_concurrent() -> usize {
    4
}

/// The recent-sessions digest knobs only — deliberately excludes
/// `[sessions] backend`, which is structural and must never be
/// workspace-overridable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDigestConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_digest_max_sessions")]
    pub max_sessions: usize,
    #[serde(default = "default_digest_window_hours")]
    pub window_hours: u64,
    #[serde(default = "default_digest_max_chars")]
    pub max_chars: usize,
}

impl Default for SessionDigestConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_sessions: default_digest_max_sessions(),
            window_hours: default_digest_window_hours(),
            max_chars: default_digest_max_chars(),
        }
    }
}

fn default_sessions_backend() -> String {
    "jsonl".to_string()
}

/// `[scheduler]`: what scheduled jobs may do. `pinned_sessions` is the
/// install's kill switch; an agent's own `config.toml` can switch it off for
/// that agent, read each time a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Whether a recurring job may keep one session across its runs. Off, a
    /// job pinned to a session runs each time in a fresh one.
    #[serde(default = "default_true")]
    pub pinned_sessions: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            pinned_sessions: true,
        }
    }
}

impl SchedulerConfig {
    /// The scheduler config an agent runs with: recurring jobs may pin a
    /// session unless the install or its own `config.toml` says otherwise.
    pub fn for_agent(&self, agent: Option<&AgentSchedulerConfig>) -> Self {
        let own = agent.and_then(|a| a.pinned_sessions);
        Self {
            pinned_sessions: self.pinned_sessions && own.unwrap_or(true),
        }
    }
}

/// `[scheduler]` in an agent's `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSchedulerConfig {
    #[serde(default)]
    pub pinned_sessions: Option<bool>,
}

/// Scheduler job store selection. Deliberately excluded from
/// `POSTGRES_DEFAULT_STORES`: jobs must be backfilled (via
/// `workforce migrate-scheduler`) before cutover, so enabling is explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerStoreConfig {
    /// Backend: `"file"` (default, per-agent `jobs.json`) or `"postgres"`.
    #[serde(default = "default_scheduler_store_backend")]
    pub backend: String,
}

impl Default for SchedulerStoreConfig {
    fn default() -> Self {
        Self {
            backend: default_scheduler_store_backend(),
        }
    }
}

fn default_scheduler_store_backend() -> String {
    "file".to_string()
}

fn default_digest_max_sessions() -> usize {
    3
}

fn default_digest_window_hours() -> u64 {
    12
}

fn default_digest_max_chars() -> usize {
    1200
}

/// Runtime artifact store configuration.
///
/// Selects between the default local-filesystem store (`fs`) and the shared
/// S3 backend (`s3`) for large tool outputs (horizontal-scaling inc2). The
/// `fs` default preserves today's local-disk + in-process behavior. The
/// `s3` backend is hub-owned: the hub builds one S3 client at boot (region +
/// optional write-role assume) and fails closed if the bucket is unreachable
/// — split-state between disk and S3 is the hazard. Deliberately excluded
/// from `POSTGRES_DEFAULT_STORES`: it is not a Postgres store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactsConfig {
    /// Backend: `"fs"` (default) or `"s3"`.
    #[serde(default = "default_artifacts_backend")]
    pub backend: String,
    /// Target bucket for the `s3` backend. Required when `backend = "s3"`.
    #[serde(default)]
    pub s3_bucket: Option<String>,
    /// Region for the `s3` backend; `None` keeps the ambient `AWS_REGION`.
    #[serde(default)]
    pub s3_region: Option<String>,
    /// Key prefix for stored objects (default `artifacts/`).
    #[serde(default = "default_artifacts_prefix")]
    pub s3_prefix: String,
    /// Optional IAM role the hub assumes (once, at boot) for the S3 writer
    /// identity. `None` uses the ambient credential chain (ECS task role).
    #[serde(default)]
    pub s3_assume_role_arn: Option<String>,
    /// Where knowledge-base artifacts are indexed for search by meaning:
    /// `"weaviate"` (default; when `[weaviate] enabled`) or `"vellev"`.
    /// Chosen apart from `[memory] backend`, so that memories can move to
    /// vellev while artifact search stays where it is.
    #[serde(default = "default_vector_backend")]
    pub index_backend: String,
    /// How long an artifact search waits for the index, in milliseconds,
    /// before it goes by keyword; the index is then left alone for half a
    /// minute, so that not every search waits.
    #[serde(default = "default_search_timeout_ms")]
    pub index_search_timeout_ms: u64,
}

/// A few seconds: longer than a healthy index takes, and short of what
/// someone asking would wait.
fn default_search_timeout_ms() -> u64 {
    3000
}

impl Default for ArtifactsConfig {
    fn default() -> Self {
        Self {
            backend: default_artifacts_backend(),
            s3_bucket: None,
            s3_region: None,
            s3_prefix: default_artifacts_prefix(),
            s3_assume_role_arn: None,
            index_backend: default_vector_backend(),
            index_search_timeout_ms: default_search_timeout_ms(),
        }
    }
}

/// Where a derived vector index is kept unless its setting says otherwise:
/// Weaviate, as before vellev could be chosen.
fn default_vector_backend() -> String {
    VECTORS_ON_WEAVIATE.to_string()
}

/// `[artifacts] index_backend` and `[knowledge] vector_backend`: Weaviate,
/// when `[weaviate] enabled`.
pub const VECTORS_ON_WEAVIATE: &str = "weaviate";
/// `[artifacts] index_backend` and `[knowledge] vector_backend`: vellev.
pub const VECTORS_ON_VELLEV: &str = "vellev";

fn default_artifacts_backend() -> String {
    "fs".to_string()
}

fn default_artifacts_prefix() -> String {
    "artifacts/".to_string()
}

/// `[revisions]` — revision-history retention for workflow / playbook /
/// identity-file / config-overlay changes
///. The history itself is
/// always on when Postgres is enabled; this only bounds its size.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionsConfig {
    /// Newest revisions kept per definition/file (delete tombstones are
    /// always kept). `0` = unlimited.
    #[serde(default = "default_revisions_max_per_def")]
    pub max_per_def: u32,
}

impl Default for RevisionsConfig {
    fn default() -> Self {
        Self {
            max_per_def: default_revisions_max_per_def(),
        }
    }
}

fn default_revisions_max_per_def() -> u32 {
    100
}

/// Identity / team store configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// Postgres backend (Phase 4 of the migration roadmap). Postgres
/// requires `[postgres] enabled = true` and the
/// `workforce-identity/postgres` cargo feature; otherwise the SQLite
/// default is used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_identity_backend")]
    pub backend: String,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            backend: default_identity_backend(),
        }
    }
}

fn default_identity_backend() -> String {
    "sqlite".to_string()
}

/// Permission cascade configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// Postgres backend (mirrors the identity/projects pattern). Disabled
/// by default while the staged rollout is in progress — when `enabled`
/// is `false` the hub does not construct a permission store and the
/// `/api/permissions/*` endpoints return `503 Service Unavailable`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    /// Whether the permission cascade is wired into the runtime + admin
    /// UI. Default: `false` so existing deployments keep using the
    /// legacy YAML-only policy path until the operator opts in.
    #[serde(default)]
    pub enabled: bool,
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_permissions_backend")]
    pub backend: String,
    /// Seed a global System-level `@destructive → Ask` baseline so any
    /// delete/remove/destroy-style tool call (including MCP) prompts for a
    /// one-time confirmation the user can Always-Allow. Active only when the
    /// cascade is enabled; more-specific rules and per-user Always-Allow tune it.
    #[serde(default = "default_true")]
    pub destructive_baseline: bool,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_permissions_backend(),
            destructive_baseline: true,
        }
    }
}

fn default_permissions_backend() -> String {
    "sqlite".to_string()
}

/// Approval-routing (designated-approvers) configuration.
///
/// Routes `RequireApproval` decisions to named approvers via Slack DMs
/// and persists a durable pending-approval row. Disabled by default
/// during rollout — when `enabled` is `false` the hub never constructs
/// the router and every approval is the existing in-channel self-serve
/// prompt. Mirrors [`PermissionsConfig`]'s staged-rollout shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalsConfig {
    /// Whether routed approvals are wired into the runtime. Default
    /// `false` so existing deployments keep self-serve approval.
    #[serde(default)]
    pub enabled: bool,
    /// Backend for the pending-approval store: `"sqlite"` (default) or
    /// `"postgres"`.
    #[serde(default = "default_approvals_backend")]
    pub backend: String,
    /// Time a routed approval stays pending before the sweeper expires
    /// it (and the requester is told it lapsed). Default: 24h.
    #[serde(default = "default_approvals_ttl_secs")]
    pub default_ttl_secs: u64,
    /// How often the expiry sweeper runs. Default: 60s.
    #[serde(default = "default_approvals_sweeper_interval_secs")]
    pub sweeper_interval_secs: u64,
    /// Days a resolved or expired request, which holds the tool call's
    /// full arguments, is kept past its expiry; `0` keeps them forever. A
    /// pending request is never pruned.
    #[serde(default = "default_approvals_retention_days")]
    pub retention_days: u32,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: default_approvals_backend(),
            default_ttl_secs: default_approvals_ttl_secs(),
            sweeper_interval_secs: default_approvals_sweeper_interval_secs(),
            retention_days: default_approvals_retention_days(),
        }
    }
}

fn default_approvals_retention_days() -> u32 {
    30
}

fn default_approvals_backend() -> String {
    "sqlite".to_string()
}

fn default_approvals_ttl_secs() -> u64 {
    86_400
}

fn default_approvals_sweeper_interval_secs() -> u64 {
    60
}

/// Pairing store configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// Postgres backend (Phase 4 of the migration roadmap). Note this is
/// distinct from the existing `pairing` block (which holds runtime
/// policy like `code_ttl_secs` and lives in `workforce-pairing`); this
/// one is purely about *where* the data is persisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingStoreConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_pairing_backend")]
    pub backend: String,
}

impl Default for PairingStoreConfig {
    fn default() -> Self {
        Self {
            backend: default_pairing_backend(),
        }
    }
}

fn default_pairing_backend() -> String {
    "sqlite".to_string()
}

/// Project store configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// Postgres backend (Phase 2 of the project-store rollout). Holds per-agent
/// project manifests; the on-disk git material (worktrees, indexer
/// checkouts, /cc-session copies) is unaffected by this setting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_projects_backend")]
    pub backend: String,
    /// Bounded concurrency for `projects.update` parallel `git pull`s.
    #[serde(default = "default_projects_update_concurrency")]
    pub update_concurrency: usize,
    /// TTL for unused `cc-sessions/{uuid}/` directories before GC reclaims
    /// them. The active session always wins; this only catches sessions
    /// whose workforce session was never closed cleanly.
    #[serde(default = "default_cc_session_ttl_secs")]
    pub cc_session_ttl_secs: u64,
    /// TTL for idle `work/session/{scope}/` workspaces — `coding.session`
    /// rounds, `projects.open_workspace`, and bench workspaces. Separate from
    /// `cc_session_ttl_secs` because these differ in kind: a cc-session is a
    /// disposable clone, while a bench's files are the deliverable, so an
    /// install may want to keep them longer.
    ///
    /// Liveness is stamped when a workspace is provisioned, so this MUST stay
    /// well above a coding round's wall clock (`[coding_session] timeout_secs`)
    /// — a shorter value could reclaim a directory Claude Code is working in.
    #[serde(default = "default_work_session_ttl_secs")]
    pub work_session_ttl_secs: u64,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            backend: default_projects_backend(),
            update_concurrency: default_projects_update_concurrency(),
            cc_session_ttl_secs: default_cc_session_ttl_secs(),
            work_session_ttl_secs: default_work_session_ttl_secs(),
        }
    }
}

fn default_projects_backend() -> String {
    "sqlite".to_string()
}

/// MCP server install persistence backend selector. SQLite default,
/// optional Postgres. The Postgres path uses the same global
/// `[postgres]` block as the project / pairing / identity stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpStoreConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_mcp_store_backend")]
    pub backend: String,
}

impl Default for McpStoreConfig {
    fn default() -> Self {
        Self {
            backend: default_mcp_store_backend(),
        }
    }
}

fn default_mcp_store_backend() -> String {
    "sqlite".to_string()
}

/// OAuth state store backend selector.
///
/// `"memory"` (default) keeps pending redirect state in-process —
/// fine for the original Google adapter where the user typically
/// completes the redirect in the same hub run.
///
/// `"sqlite"` persists pending state to `oauth_state.db` under the
/// hub's data dir so a UI-initiated MCP-install OAuth flow can
/// complete its callback even if the hub bounced in between.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthStateStoreConfig {
    /// Backend: `"memory"` (default) or `"sqlite"`.
    #[serde(default = "default_oauth_state_backend")]
    pub backend: String,
}

impl Default for OAuthStateStoreConfig {
    fn default() -> Self {
        Self {
            backend: default_oauth_state_backend(),
        }
    }
}

fn default_oauth_state_backend() -> String {
    "memory".to_string()
}

fn default_projects_update_concurrency() -> usize {
    4
}

fn default_cc_session_ttl_secs() -> u64 {
    7 * 24 * 60 * 60
}

fn default_work_session_ttl_secs() -> u64 {
    7 * 24 * 60 * 60
}

/// Code graph store configuration.
///
/// Selects between the default embedded SQLite store and the optional
/// Postgres backend (Phase 3 of the migration roadmap). Vector search
/// (code/summary embeddings) stays on Weaviate regardless of this
/// setting — only the structural metadata graph moves to Postgres.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodegraphConfig {
    /// Backend: `"sqlite"` (default) or `"postgres"`.
    #[serde(default = "default_codegraph_backend")]
    pub backend: String,
}

impl Default for CodegraphConfig {
    fn default() -> Self {
        Self {
            backend: default_codegraph_backend(),
        }
    }
}

fn default_codegraph_backend() -> String {
    "sqlite".to_string()
}

fn default_memory_db_path() -> String {
    "data/memory.db".to_string()
}

fn default_embedding_provider() -> String {
    "none".to_string()
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

fn default_embedding_api_url() -> String {
    "https://api.openai.com/v1/embeddings".to_string()
}

/// Codebase Knowledge Engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeConfig {
    /// Whether the knowledge engine is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Root directory to index. If unset, defaults to the agent's workspace.
    #[serde(default)]
    pub root: Option<String>,
    /// Directory for knowledge engine data (SQLite DB, index state).
    /// Separate from `root` (what to index) so the DB can be on a different volume.
    /// Defaults to `root` if not set.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Whether to trigger indexing on startup.
    #[serde(default)]
    pub index_on_startup: bool,
    /// Number of concurrent file parsing workers. 0 = auto (num_cpus * 2).
    #[serde(default)]
    pub parse_concurrency: usize,
    /// Number of concurrent LLM summary calls during indexing. 0 = auto (16).
    /// Summary generation is network-bound, so this can be much higher
    /// than `parse_concurrency`.
    #[serde(default)]
    pub summary_concurrency: usize,
    /// Whether to generate LLM summaries during indexing.
    #[serde(default)]
    pub generate_summaries: bool,
    /// Additional glob patterns to exclude from indexing.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Maximum file size in bytes (default: 1MB).
    #[serde(default = "default_max_file_size")]
    pub max_file_size: u64,
    /// Periodic maintenance interval in seconds (default: 14400 = 4 hours).
    #[serde(default = "default_maintenance_interval")]
    pub maintenance_interval_secs: u64,
    /// Maximum time in seconds for an indexing operation before timeout (default: 1800 = 30 min).
    #[serde(default = "default_index_timeout")]
    pub index_timeout_secs: u64,
    /// Summariser configuration.
    #[serde(default)]
    pub summariser: KnowledgeSummariserConfig,
    /// Where code vectors are kept: `"weaviate"` (default; when `[weaviate]
    /// enabled`) or `"vellev"`. Chosen apart from `[memory] backend`, so
    /// that memories can move to vellev while code search stays where it is.
    #[serde(default = "default_vector_backend")]
    pub vector_backend: String,
    /// A vector database kept in step with `vector_backend` as a standby:
    /// `""` (none, the default), `"weaviate"` or `"vellev"`, and not the
    /// one in use. It is never filled nor searched: every file an index run
    /// changes is noted pending for it, so that once it is switched to, it
    /// is brought up to date file by file rather than filled again whole.
    /// Set it while moving code vectors from one database to the other,
    /// and clear it once the old one is retired.
    #[serde(default)]
    pub vector_standby: String,
    /// The most code entities and summaries per second the hub's own
    /// backfill of code vectors has the vector database embed; `0` for no
    /// limit. A vellev node runs its model on one batch at a time and a
    /// question waits for the batch under way, so the backfill sends small
    /// batches with gaps between them: it keeps the vectors up, and is not
    /// how a large graph is first filled (`workforce code reindex-vectors`
    /// is).
    #[serde(default = "default_vector_backfill_per_sec")]
    pub vector_backfill_per_sec: u32,
    /// How long a code question waits for the vector store, in
    /// milliseconds, before it is answered from the graph alone; the store
    /// is then left alone for half a minute.
    #[serde(default = "default_search_timeout_ms")]
    pub vector_search_timeout_ms: u64,
    /// How often, in seconds, each repo's code vectors are compared with
    /// the code graph, entity by entity, and put right: what finds vectors
    /// that fell behind with nothing noted, as after the vector database
    /// was restored from a backup. Also done when a hub starts.
    #[serde(default = "default_vector_reconcile_secs")]
    pub vector_reconcile_secs: u64,
}

/// Six hours: a comparison of millions of entities takes tens of minutes
/// at its pace.
fn default_vector_reconcile_secs() -> u64 {
    6 * 3600
}

/// A third or less of what a node's bge embeds when nothing else asks it
/// (11 to 18 a second measured on a busy laptop), so
/// that most questions find the model free.
fn default_vector_backfill_per_sec() -> u32 {
    5
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: None,
            data_dir: None,
            index_on_startup: false,
            parse_concurrency: 0,
            summary_concurrency: 0,
            generate_summaries: false,
            exclude_patterns: Vec::new(),
            max_file_size: default_max_file_size(),
            maintenance_interval_secs: default_maintenance_interval(),
            index_timeout_secs: default_index_timeout(),
            summariser: KnowledgeSummariserConfig::default(),
            vector_backend: default_vector_backend(),
            vector_standby: String::new(),
            vector_backfill_per_sec: default_vector_backfill_per_sec(),
            vector_search_timeout_ms: default_search_timeout_ms(),
            vector_reconcile_secs: default_vector_reconcile_secs(),
        }
    }
}

fn default_max_file_size() -> u64 {
    1_048_576
}

fn default_maintenance_interval() -> u64 {
    14400
}

fn default_index_timeout() -> u64 {
    1800
}

/// Configuration for the LLM summariser used by the knowledge engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSummariserConfig {
    /// LLM provider for summaries: "ollama", "anthropic", "openrouter",
    /// "gemini", "grok", "vllm", "cerebras", "bedrock".
    /// Ignored when `model` uses provider:tier shorthand (e.g. `openrouter:deepseek:powerful`).
    #[serde(default = "default_summariser_provider")]
    pub provider: String,
    /// Base URL for the summariser LLM endpoint.
    /// For Ollama: `http://localhost:11434`, for vLLM: `http://localhost:8000`, etc.
    #[serde(default = "default_summariser_url", alias = "ollama_url")]
    pub url: String,
    /// Model name for summary generation.
    /// Supports tier shorthand: `openrouter:deepseek:powerful`, `gemini:standard`, etc.
    /// When using shorthand, the `provider` field is ignored and the provider is derived
    /// from the model spec. A bare tier name (`cheap`) resolves against the `provider`
    /// field's tier table. The API key is read on every summary from the global
    /// vault's `knowledge/summariser/api_key`, then from `api_key`,
    /// `SUMMARISER_API_KEY` or the provider-specific env var (e.g.
    /// `OPENROUTER_API_KEY`, `GEMINI_API_KEY`).
    #[serde(default = "default_summariser_model")]
    pub model: String,
    /// Optional API key for authenticated endpoints, read when the global
    /// vault holds none. Typically set via the `SUMMARISER_API_KEY` env var.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for KnowledgeSummariserConfig {
    fn default() -> Self {
        Self {
            provider: default_summariser_provider(),
            url: default_summariser_url(),
            model: default_summariser_model(),
            api_key: None,
        }
    }
}

fn default_summariser_provider() -> String {
    "ollama".to_string()
}

fn default_summariser_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_summariser_model() -> String {
    "qwen2.5-coder:7b".to_string()
}

/// LLM provider configuration for image and PDF attachment routing.
///
/// When the active agent's primary model isn't vision-capable, image
/// attachments are routed through the LLM described here, the response is
/// substituted as a text content block, and the original image is dropped.
/// PDFs use a hybrid path: text extraction first, vision page-render fallback
/// for scanned/image-heavy documents.
///
/// Whole-block override semantics: if an agent supplies its own `[llm.vision]`
/// section, none of the global `[llm.vision]` fields leak through. The
/// sub-fields (provider, url, api_key) are tightly coupled and inheriting them
/// piecewise produces broken hybrid configs.
///
/// ```toml
/// [llm.vision]
/// model = "openrouter:anthropic:cheap"   # tier shorthand resolves Haiku via OpenRouter
/// # api_key resolved from OPENROUTER_API_KEY env
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmVisionConfig {
    /// Enable vision fallback. When false, image and PDF attachments pass
    /// through unchanged even to non-vision-capable providers (legacy behaviour).
    #[serde(default = "default_vision_enabled")]
    pub enabled: bool,
    /// Provider id of a bare `model` ("anthropic", "openrouter:anthropic",
    /// "gemini", "vllm", "bedrock", "chatgpt"): one whose requests carry
    /// images. Ignored when `model` names its own provider (`provider:tier`,
    /// `provider:model`); a bare `model` without it is refused, never given
    /// a provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Unused: no provider whose requests carry images takes a base URL
    /// here. Kept so that configs that set it still load.
    #[serde(default)]
    pub url: Option<String>,
    /// Model spec. Tier shorthand supported: `openrouter:anthropic:cheap`,
    /// `gemini:standard`, `anthropic:cheap`, etc. Resolved against the vision
    /// tier registry (`default_vision_tiers.toml`).
    pub model: String,
    /// Optional API key, read when no vault level holds
    /// `vision/<provider>/api_key` for the person a description is for. Falls
    /// back to `VISION_API_KEY`, then the provider-specific env var
    /// (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// In-memory description cache size, keyed by SHA-256 of the base64 image
    /// payload. 0 disables caching.
    #[serde(default = "default_vision_cache_size")]
    pub cache_size: usize,
    /// Minimum extracted-text length before a PDF is treated as text-extractable.
    /// Below this, the hybrid pipeline falls through to per-page vision rendering.
    #[serde(default = "default_pdf_text_min_chars")]
    pub pdf_text_min_chars: usize,
    /// Maximum PDF pages to render via vision when falling back from text
    /// extraction. Bounds vision-call cost on large slide decks / scanned reports.
    #[serde(default = "default_pdf_max_pages")]
    pub pdf_max_pages: usize,
}

impl Default for LlmVisionConfig {
    fn default() -> Self {
        Self {
            enabled: default_vision_enabled(),
            provider: None,
            url: None,
            model: String::new(),
            api_key: None,
            cache_size: default_vision_cache_size(),
            pdf_text_min_chars: default_pdf_text_min_chars(),
            pdf_max_pages: default_pdf_max_pages(),
        }
    }
}

fn default_vision_enabled() -> bool {
    true
}
fn default_vision_cache_size() -> usize {
    256
}
fn default_pdf_text_min_chars() -> usize {
    200
}
fn default_pdf_max_pages() -> usize {
    20
}

/// LLM provider configuration for audio and video attachment transcription.
///
/// When the agent receives an audio attachment (voice memo, MP3, M4A, etc.)
/// it's routed through the LLM described here, the transcript is substituted
/// as a text content block, and the original file is dropped from the prompt.
/// Video attachments have their audio track extracted via `ffmpeg` and follow
/// the same path. No agent's primary chat model needs to support audio for
/// this to work.
///
/// Whole-block override semantics: if an agent supplies its own
/// `[llm.transcription]` section, none of the global fields leak through.
///
/// ```toml
/// [llm.transcription]
/// model = "openrouter:google:cheap"   # tier shorthand resolves Gemini Flash Lite via OpenRouter
/// # api_key resolved from OPENROUTER_API_KEY env
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTranscriptionConfig {
    /// Enable transcription. When false, audio/video attachments fall through
    /// to the saved-to-workspace path (legacy behaviour).
    #[serde(default = "default_transcription_enabled")]
    pub enabled: bool,
    /// Provider id of a bare `model` ("openai", "groq", "gemini",
    /// "openrouter:google", "vllm", "custom"). Ignored when `model` names its
    /// own provider (`provider:tier`, `provider:model`); a bare `model`
    /// without it is refused, never given a provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Base URL override (vLLM, custom Whisper deployments).
    #[serde(default)]
    pub url: Option<String>,
    /// Model spec. Tier shorthand supported: `groq:cheap`, `openai:standard`,
    /// `openrouter:google:cheap`, `gemini:standard`, etc. Resolved against the
    /// transcription tier registry.
    pub model: String,
    /// Optional API key, read when no vault level holds
    /// `transcription/<provider>/api_key` for the person a transcription is
    /// for. Falls back to `TRANSCRIPTION_API_KEY`, then the provider-specific
    /// env var (`OPENAI_API_KEY`, `GROQ_API_KEY`, `OPENROUTER_API_KEY`,
    /// `GEMINI_API_KEY`), then the provider's `llm/accounts/*` entry.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional ISO-639-1 language hint passed to providers that accept one.
    /// Omit for auto-detect.
    #[serde(default)]
    pub language: Option<String>,
    /// In-memory transcript cache size, keyed by SHA-256 of the audio bytes.
    /// 0 disables caching.
    #[serde(default = "default_transcription_cache_size")]
    pub cache_size: usize,
    /// Refuse audio longer than this many minutes. Bounds worst-case
    /// transcription spend on very long uploads; media under the cap but over
    /// `chunk_minutes` is transcribed in segments, so this is a total budget,
    /// not a per-request payload ceiling.
    #[serde(default = "default_max_audio_minutes")]
    pub max_audio_minutes: usize,
    /// Segment length for long media: anything longer than this is split with
    /// `ffmpeg -f segment` into chunks of this many minutes, each transcribed
    /// separately and stitched with time offsets — one provider request never
    /// carries more than a chunk's worth of audio. 0 disables chunking
    /// (whole file in a single request, as before).
    #[serde(default = "default_transcription_chunk_minutes")]
    pub chunk_minutes: usize,
    /// When true, video attachments have their audio track extracted with
    /// `ffmpeg` and routed through the audio pipeline. What the video shows
    /// is `[attachments.video]` ([`VideoFramesConfig`]).
    #[serde(default = "default_transcribe_video_audio")]
    pub transcribe_video_audio: bool,
}

impl Default for LlmTranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: default_transcription_enabled(),
            provider: None,
            url: None,
            model: String::new(),
            api_key: None,
            language: None,
            cache_size: default_transcription_cache_size(),
            max_audio_minutes: default_max_audio_minutes(),
            chunk_minutes: default_transcription_chunk_minutes(),
            transcribe_video_audio: default_transcribe_video_audio(),
        }
    }
}

fn default_transcription_enabled() -> bool {
    true
}
fn default_transcription_cache_size() -> usize {
    256
}
/// 240 minutes: with chunked transcription the per-request payload is bounded
/// by `chunk_minutes`, so this only bounds total spend per attachment.
fn default_max_audio_minutes() -> usize {
    240
}
fn default_transcription_chunk_minutes() -> usize {
    10
}
fn default_transcribe_video_audio() -> bool {
    true
}

/// `[attachments]`: how attachments become model input, beyond which model
/// reads them (that is `[llm.vision]` / `[llm.transcription]`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AttachmentsConfig {
    #[serde(default)]
    pub video: VideoFramesConfig,
}

/// `[attachments.video]`: still frames sampled from a video attachment so the
/// model sees what it shows, next to the transcript of what it says. Frames
/// render like image attachments — natively, described by `[llm.vision]`, or
/// not at all when neither can see. Every field may also be set in an agent's
/// own `config.toml` ([`AgentVideoFramesConfig`]), read each turn; `enabled`
/// here is the install's kill switch.
///
/// ```toml
/// [attachments.video]
/// max_frames = 8
/// max_long_edge = 1024
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoFramesConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Frames sampled from one video at most. Clamped to
    /// [`VideoFramesConfig::FRAMES_CEILING`].
    #[serde(default = "default_video_max_frames")]
    pub max_frames: usize,
    /// Seconds between sampled frames at least, so a short clip yields a few
    /// distinct frames rather than `max_frames` near-duplicates.
    #[serde(default = "default_video_min_interval_secs")]
    pub min_interval_secs: f64,
    /// Videos longer than this many minutes get no frames, only a note.
    #[serde(default = "default_video_max_minutes")]
    pub max_minutes: usize,
    /// Longest edge of a frame in pixels; smaller videos are not upscaled.
    #[serde(default = "default_video_max_long_edge")]
    pub max_long_edge: u32,
    /// Longest edge of a frame the agent re-opens a video to see in detail
    /// (`view_attachment` with `detail`), so small things in it are legible.
    /// Never below `max_long_edge`.
    #[serde(default = "default_video_detail_long_edge")]
    pub detail_long_edge: u32,
    /// JPEG quality of a frame, 1 (smallest) to 100 (best).
    #[serde(default = "default_video_jpeg_quality")]
    pub jpeg_quality: u8,
    /// How different (0..1) consecutive keyframes must be to count as a scene
    /// change; a sampled frame moves to a nearby scene change so it shows the
    /// new shot rather than a transition. 0 disables the scan.
    #[serde(default = "default_video_scene_threshold")]
    pub scene_threshold: f64,
}

impl Default for VideoFramesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_frames: default_video_max_frames(),
            min_interval_secs: default_video_min_interval_secs(),
            max_minutes: default_video_max_minutes(),
            max_long_edge: default_video_max_long_edge(),
            detail_long_edge: default_video_detail_long_edge(),
            jpeg_quality: default_video_jpeg_quality(),
            scene_threshold: default_video_scene_threshold(),
        }
    }
}

impl VideoFramesConfig {
    /// Frames per video no config can exceed: each one is an image resent on
    /// every later round of the turn, and an Anthropic-shaped request keeps
    /// to 20 images (older frames give way past that).
    pub const FRAMES_CEILING: usize = 20;
    /// Frame edge no config can exceed: what standard-resolution Claude
    /// models see natively (newer ones take 2576), under the 2000px a request
    /// with over 20 images allows, and within every other provider's limits.
    pub const LONG_EDGE_CEILING: u32 = 1568;
    const LONG_EDGE_FLOOR: u32 = 128;

    /// The frame config this agent runs with, its own `config.toml` overriding
    /// this field by field and every value clamped to a sane range; `None`
    /// when the install or the agent turns frames off.
    pub fn for_agent(&self, agent: Option<&AgentVideoFramesConfig>) -> Option<Self> {
        let agent = agent.cloned().unwrap_or_default();
        if !self.enabled || agent.enabled == Some(false) {
            return None;
        }
        let finite_or =
            |v: Option<f64>, fallback: f64| v.filter(|v| v.is_finite()).unwrap_or(fallback);
        let max_long_edge = agent
            .max_long_edge
            .unwrap_or(self.max_long_edge)
            .clamp(Self::LONG_EDGE_FLOOR, Self::LONG_EDGE_CEILING);
        Some(Self {
            enabled: true,
            max_frames: agent
                .max_frames
                .unwrap_or(self.max_frames)
                .clamp(1, Self::FRAMES_CEILING),
            min_interval_secs: finite_or(agent.min_interval_secs, self.min_interval_secs).max(0.0),
            max_minutes: agent.max_minutes.unwrap_or(self.max_minutes),
            max_long_edge,
            detail_long_edge: agent
                .detail_long_edge
                .unwrap_or(self.detail_long_edge)
                .clamp(max_long_edge, Self::LONG_EDGE_CEILING),
            jpeg_quality: agent
                .jpeg_quality
                .unwrap_or(self.jpeg_quality)
                .clamp(1, 100),
            scene_threshold: finite_or(agent.scene_threshold, self.scene_threshold).clamp(0.0, 1.0),
        })
    }
}

/// `[attachments]` in an agent's `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentAttachmentsConfig {
    #[serde(default)]
    pub video: Option<AgentVideoFramesConfig>,
}

/// `[attachments.video]` in an agent's `config.toml`; see [`VideoFramesConfig`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentVideoFramesConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub max_frames: Option<usize>,
    #[serde(default)]
    pub min_interval_secs: Option<f64>,
    #[serde(default)]
    pub max_minutes: Option<usize>,
    #[serde(default)]
    pub max_long_edge: Option<u32>,
    #[serde(default)]
    pub detail_long_edge: Option<u32>,
    #[serde(default)]
    pub jpeg_quality: Option<u8>,
    #[serde(default)]
    pub scene_threshold: Option<f64>,
}

fn default_video_max_frames() -> usize {
    8
}
fn default_video_min_interval_secs() -> f64 {
    2.0
}
fn default_video_max_minutes() -> usize {
    10
}
fn default_video_max_long_edge() -> u32 {
    1024
}
fn default_video_detail_long_edge() -> u32 {
    VideoFramesConfig::LONG_EDGE_CEILING
}
fn default_video_jpeg_quality() -> u8 {
    80
}
fn default_video_scene_threshold() -> f64 {
    0.3
}

/// Image-generation provider configuration.
///
/// Sibling of [`LlmVisionConfig`] / [`LlmTranscriptionConfig`]: a secondary
/// modality provider configured per-install and routed by the same
/// `provider:upstream:tier` shorthand. Unlike vision/transcription there is no
/// universal default model and no chat provider can be reused (Cerebras is
/// text-only, Anthropic has no image generation, and even on Google the image
/// model differs from the chat model) — so image generation is strictly
/// opt-in: when this block is absent the generate-image tool is not registered.
///
/// Whole-block override semantics: if an agent supplies its own `[llm.image]`
/// section, none of the global fields leak through.
///
/// ```toml
/// [llm.image]
/// model = "bedrock:stability:standard"   # tier shorthand → Stable Image Ultra
/// # api_key resolved from the provider-specific env var
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmImageGenerationConfig {
    /// Enable image generation. Default true *when the block is present* — the
    /// opt-in gate is the presence of the block itself, this toggle lets an
    /// install keep the config but switch generation off without deleting it.
    #[serde(default = "default_image_enabled")]
    pub enabled: bool,
    /// Provider id of a bare `model` ("gemini", "bedrock:stability",
    /// "openrouter:google", "vllm"). Ignored when `model` names its own
    /// provider (`provider:tier`, `provider:model`); a bare `model` without
    /// it is refused, never given a provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Base URL override (self-hosted FLUX/ComfyUI, custom endpoints).
    #[serde(default)]
    pub url: Option<String>,
    /// Model spec. Tier shorthand supported: `bedrock:stability:standard`,
    /// `gemini:standard`, `openrouter:google:standard`. Resolved against the
    /// image tier registry (`default_image_tiers.toml`).
    pub model: String,
    /// Optional API key, read when no vault level holds
    /// `image/<provider>/api_key` for the person an image is for. Falls back
    /// to `IMAGE_API_KEY`, then the provider's `llm/accounts/*` entry, its
    /// credential chain (Bedrock) or its env var (`GEMINI_API_KEY`,
    /// `OPENROUTER_API_KEY`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default output resolution as `WxH`, e.g. `"1024x1024"`. Providers clamp
    /// to the nearest supported size.
    #[serde(default = "default_image_size")]
    pub size: String,
    /// Default number of images per request. Most callers want 1.
    #[serde(default = "default_image_count")]
    pub n: u8,
    /// Region override for region-bound providers (Bedrock). Lets image
    /// generation run in a different region than chat — e.g. chat on
    /// eu-west-2 but Bedrock image models only exist in us-east-1. Ignored by
    /// providers that aren't region-scoped. Defaults to the chat region.
    #[serde(default)]
    pub region: Option<String>,
}

impl Default for LlmImageGenerationConfig {
    fn default() -> Self {
        Self {
            enabled: default_image_enabled(),
            provider: None,
            url: None,
            model: String::new(),
            api_key: None,
            size: default_image_size(),
            n: default_image_count(),
            region: None,
        }
    }
}

fn default_image_enabled() -> bool {
    true
}
fn default_image_size() -> String {
    "1024x1024".to_string()
}
fn default_image_count() -> u8 {
    1
}

/// Video-generation provider configuration (`[llm.video]`), sibling to
/// `[llm.image]`. Opt-in per install — absent block ⇒ no `generate_video` tool.
///
/// Today the only backend is Google Veo via the Gemini API (`predictLongRunning`
/// submit → poll → download), so `model` resolves against `default_video_tiers.toml`
/// (e.g. `veo:standard`). The credential resolves like image gen
/// (the vault's `video/gemini/api_key` for the person asking → `api_key` →
/// `VIDEO_API_KEY` → vault account → `GEMINI_API_KEY`).
///
/// ```toml
/// [llm.video]
/// model = "veo:cheap"           # tier shorthand → veo-3.1-lite-generate-preview
/// person_generation = "allow_adult"   # required to animate a real person's face
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmVideoGenerationConfig {
    /// Enable video generation. Default true when the block is present.
    #[serde(default = "default_video_enabled")]
    pub enabled: bool,
    /// Provider id ("veo", or "gemini", whose API serves it): the only video
    /// provider, so a bare `model` is Veo's without it.
    #[serde(default)]
    pub provider: Option<String>,
    /// Base URL override (unused for Veo; reserved for self-hosted backends).
    #[serde(default)]
    pub url: Option<String>,
    /// Model spec. Tier shorthand (`veo:standard`, `veo:cheap`) resolved against
    /// the video tier registry (`default_video_tiers.toml`), or a Veo model id.
    pub model: String,
    /// Optional API key, read when no vault level holds
    /// `video/gemini/api_key` for the person a clip is for. Falls back to
    /// `VIDEO_API_KEY`, then the vault account, then the provider env var
    /// (`GEMINI_API_KEY`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Aspect ratio for generated clips, e.g. `"16:9"` or `"9:16"`.
    #[serde(default = "default_video_aspect")]
    pub aspect_ratio: String,
    /// Requested clip duration in seconds (model-clamped).
    #[serde(default)]
    pub duration_secs: Option<u32>,
    /// People-generation safety knob. `"allow_adult"` is required to animate a
    /// real person's face on Veo image-to-video; `"dont_allow"` blocks people.
    #[serde(default = "default_person_generation")]
    pub person_generation: String,
    /// Region override for region-bound providers (reserved; Veo is global).
    #[serde(default)]
    pub region: Option<String>,
}

impl Default for LlmVideoGenerationConfig {
    fn default() -> Self {
        Self {
            enabled: default_video_enabled(),
            provider: None,
            url: None,
            model: String::new(),
            api_key: None,
            aspect_ratio: default_video_aspect(),
            duration_secs: None,
            person_generation: default_person_generation(),
            region: None,
        }
    }
}

fn default_video_enabled() -> bool {
    true
}
fn default_video_aspect() -> String {
    "16:9".to_string()
}
fn default_person_generation() -> String {
    "allow_adult".to_string()
}

/// Speech-synthesis (text-to-speech) provider configuration (`[llm.speech]`),
/// sibling to `[llm.image]` / `[llm.video]`. Opt-in per install — absent block
/// ⇒ no `generate_speech` tool. Generated clips are staged as artifacts and
/// delivered to the active channel surface (Slack file upload) like generated
/// images.
///
/// Unlike the other modality blocks there is no tier registry: TTS "models"
/// are voices, which don't map onto cheap/standard/powerful tiers — so the
/// provider is named explicitly and `model`/`voice` are plain provider ids.
///
/// Providers mirror the mobile app's TTS options:
///   - `"builtin"`    — Piper, runs locally as a subprocess; the binary and
///     the voice model file are downloaded on first use (no API key).
///   - `"fish_audio"` — Fish Audio HTTP API (`FISH_AUDIO_API_KEY`).
///   - `"elevenlabs"` — ElevenLabs HTTP API (`ELEVENLABS_API_KEY`).
///   - `"openai"`     — OpenAI speech API (`OPENAI_API_KEY`).
///   - `"soniox"`     — Soniox HTTP API (`SONIOX_API_KEY`); voices are
///     multilingual, `language` selects pronunciation.
///
/// Credential resolution, on every clip: the vault secret
/// `speech/<provider>/api_key` (the PLAIN key string, no JSON) at the level
/// that holds it for the person speaking (their own, the agent's, the
/// install's) → `api_key` → `SPEECH_API_KEY` → a matching `llm/accounts/*`
/// account entry (legacy/openai) → the provider env var above. `"builtin"`
/// needs no credential.
///
/// ```toml
/// [llm.speech]
/// provider = "fish_audio"
/// voice = "b545c585f631496c914815291da4e893"   # fish reference_id
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmSpeechConfig {
    /// Enable speech synthesis. Default true when the block is present — the
    /// opt-in gate is the presence of the block itself.
    #[serde(default = "default_speech_enabled")]
    pub enabled: bool,
    /// Provider id: "builtin" (Piper), "fish_audio", "elevenlabs", "openai",
    /// "soniox".
    pub provider: String,
    /// Provider model. Defaults per provider: openai `gpt-4o-mini-tts`,
    /// elevenlabs `eleven_multilingual_v2`, fish_audio `s1`, soniox
    /// `tts-rt-v1`. Unused by `builtin` (the voice id names the model file).
    #[serde(default)]
    pub model: Option<String>,
    /// Voice: openai voice name (`alloy`, `nova`, …), elevenlabs voice id,
    /// fish_audio reference_id (omit for the model's default voice), soniox
    /// voice name (`Maya`, `Adrian`, …; defaults to `Maya`), builtin
    /// Piper voice id (`pt_BR-faber-medium`; defaults to
    /// `en_US-lessac-medium`).
    #[serde(default)]
    pub voice: Option<String>,
    /// ISO code of the language the agent speaks in clips, e.g. `"el"`.
    /// Currently soniox-only (its voices are multilingual and the request
    /// names the input language explicitly; defaults to `"en"`); the other
    /// providers detect or infer language from the text.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional API key, read when no vault level holds
    /// `speech/<provider>/api_key`. Falls back to `SPEECH_API_KEY`, then a
    /// matching `llm/accounts/*` entry, then the provider-specific env var.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Base URL override (OpenAI-compatible endpoints, self-hosted gateways).
    #[serde(default)]
    pub url: Option<String>,
    /// Output format. `"mp3"` (default) or `"wav"`. `builtin` always emits
    /// wav (Piper's native output).
    #[serde(default = "default_speech_format")]
    pub format: String,
    /// Default speaking-rate multiplier (1.0 = normal). All providers honour
    /// it (openai `speed`, elevenlabs `voice_settings.speed`, fish_audio
    /// `prosody.speed`, soniox `speed`, builtin `--length_scale`), each
    /// clamping to its own range. The model can override per clip via the
    /// `generate_speech` tool's `speed` parameter.
    #[serde(default)]
    pub speed: Option<f32>,
    /// `builtin` only: directory for the downloaded Piper binary and voice
    /// files. Defaults to `{workspace}/piper`.
    #[serde(default)]
    pub data_dir: Option<String>,
}

impl Default for LlmSpeechConfig {
    fn default() -> Self {
        Self {
            enabled: default_speech_enabled(),
            provider: String::new(),
            model: None,
            voice: None,
            language: None,
            api_key: None,
            url: None,
            format: default_speech_format(),
            speed: None,
            data_dir: None,
        }
    }
}

fn default_speech_enabled() -> bool {
    true
}
fn default_speech_format() -> String {
    "mp3".to_string()
}

/// The names that once selected the removed `claude-max` provider, which
/// sent a Claude subscription token from the hub's own client.
pub const REMOVED_PROVIDER_ALIASES: &[&str] = &[
    "claude-max",
    "claudemax",
    "max",
    "pro-max",
    "promax",
    "subscription",
    "oauth",
];
/// What a config naming a removed provider, or handing a Claude
/// subscription token to the API client, is told to use instead.
pub const REMOVED_PROVIDER_POINTER: &str = "a Claude plan runs only through the claude CLI. Use \
     `provider = \"claude-code\"` (e.g. `default_model = \"claude-code:sonnet\"`) and store the \
     login with `claude_code.setup_token`";
/// What a Claude subscription's OAuth token starts with, whatever its
/// version (`sk-ant-oat01-…`).
pub const CLAUDE_OAT_PREFIX: &str = "sk-ant-oat";

/// Whether `name` names the removed `claude-max` provider.
pub fn is_removed_provider(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    REMOVED_PROVIDER_ALIASES.contains(&name.as_str())
}

/// Whether `key` is a Claude subscription's OAuth token rather than an API
/// key.
pub fn is_claude_oat(key: &str) -> bool {
    key.trim_start().starts_with(CLAUDE_OAT_PREFIX)
}

/// What `subject`, a Claude subscription token handed over as an API key,
/// is refused with.
pub fn oat_refusal(subject: &str) -> String {
    format!(
        "{subject} is a Claude subscription token ({CLAUDE_OAT_PREFIX}…), not an API key: {REMOVED_PROVIDER_POINTER}"
    )
}

/// LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Provider: "anthropic", "openrouter", "grok", "ollama", "gemini",
    /// "vllm", "cerebras", "zai", "bedrock", "vertex-anthropic", or a
    /// subscription route: "chatgpt", "claude-code" or "codex".
    /// vllm connects to a self-hosted vLLM server (set VLLM_URL and VLLM_VISION_URL env vars).
    /// bedrock uses AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN
    /// and AWS_REGION (or BEDROCK_REGION); supports Claude, GLM, DeepSeek,
    /// Qwen, Kimi, MiniMax, Llama, and Mistral families.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// API key.
    /// For anthropic: standard API key (starts with sk-ant-api).
    /// For openrouter: OpenRouter API key (starts with sk-or-).
    /// For bedrock: optional `ACCESS_KEY:SECRET[:SESSION_TOKEN]` packed string;
    /// defaults to the AWS env vars when empty.
    /// Prefer env vars: ANTHROPIC_API_KEY, OPENROUTER_API_KEY, or
    /// AWS_ACCESS_KEY_ID + AWS_SECRET_ACCESS_KEY.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default model.
    #[serde(default = "default_model")]
    pub default_model: String,
    /// Max tokens per response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Max tool execution rounds.
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    /// Max tool execution rounds for interactive (human-conversation) turns.
    /// Automated turns (scheduler, monitor) keep `max_rounds`; a human waiting
    /// on a reply gets this tighter budget, and the wind-down summary invites
    /// them to say "continue" to resume. Explicit per-call and runtime
    /// overrides still win.
    #[serde(default = "default_interactive_max_rounds")]
    pub interactive_max_rounds: u32,
    /// Ordered fallback models to try when the primary fails.
    #[serde(default)]
    pub failover: Vec<String>,
    /// Model aliases: friendly name → full model identifier.
    #[serde(default)]
    pub aliases: std::collections::HashMap<String, String>,
    /// Retry configuration for transient errors.
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    /// Proactive TPM pacing for the provider pool.
    #[serde(default)]
    pub pacing: PacingConfig,
    /// Model health tracking configuration.
    #[serde(default)]
    pub health: Option<ModelHealthConfig>,
    /// Additional accounts for the primary provider (env var names).
    /// Each env var should contain an API key.
    /// Example: `["ANTHROPIC_API_KEY_2", "ANTHROPIC_API_KEY_3"]`
    #[serde(default)]
    pub accounts: Vec<String>,
    /// Extended thinking token budget. When set, the model can reason
    /// internally before producing tool calls or text output.
    /// Significantly improves quality for complex multi-step tasks.
    /// Requires `max_tokens >= thinking_budget_tokens`.
    /// Example: `thinking_budget_tokens = 10000`
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// Per-model max_tokens overrides. When the active model matches a key
    /// (exact match first, then substring), this value is used instead of
    /// the global `max_tokens`.
    ///
    /// ```toml
    /// [llm.model_max_tokens]
    /// "claude-opus-4-6" = 128000
    /// "claude-sonnet-4-5-20250929" = 64000
    /// ```
    #[serde(default)]
    pub model_max_tokens: std::collections::HashMap<String, u32>,
    /// Fetch model pricing from provider APIs at startup (default: false).
    /// Currently supports OpenRouter (`/api/v1/models`).
    /// Fetched prices override TOML defaults and are cached in SQLite.
    #[serde(default)]
    pub fetch_pricing: bool,
    /// Use the Z.ai Coding Plan subscription endpoint instead of pay-as-you-go.
    /// Only applies when provider is "zai". Default: false.
    #[serde(default)]
    pub zai_subscription: bool,
    /// Vision fallback for non-vision-capable primary models. When set, image
    /// and PDF attachments are routed through this LLM and the resulting
    /// description is substituted in place of the raw image block.
    #[serde(default)]
    pub vision: Option<LlmVisionConfig>,
    /// Transcription provider for audio and video attachments. When set,
    /// audio is transcribed and the transcript is substituted as a text
    /// block; video has its audio track extracted and follows the same path.
    #[serde(default)]
    pub transcription: Option<LlmTranscriptionConfig>,
    /// Image-generation provider. When set, the generate-image tool is
    /// registered and routes to the configured provider; generated images are
    /// delivered to the active channel surface (Slack file upload, gateway
    /// blob attachment). Absent ⇒ no image tool (opt-in per install).
    #[serde(default)]
    pub image: Option<LlmImageGenerationConfig>,
    /// Video-generation provider (`[llm.video]`). When set, the `generate_video`
    /// tool is registered (Google Veo). Absent ⇒ no video tool (opt-in).
    #[serde(default)]
    pub video: Option<LlmVideoGenerationConfig>,
    /// Speech-synthesis provider (`[llm.speech]`). When set, the
    /// `generate_speech` tool is registered; generated voice clips are
    /// delivered to the active channel surface. Absent ⇒ no speech tool
    /// (opt-in per install).
    #[serde(default)]
    pub speech: Option<LlmSpeechConfig>,
    /// Which subscription logins may serve a `claude-code`, `codex` or
    /// `chatgpt` route (`[llm.logins]`).
    #[serde(default)]
    pub logins: LoginPolicy,
}

/// Where an agent looks for a subscription login and whom its shared login
/// may serve (`[llm.logins]`). By default every tier is tried, the payer's
/// own logins first, and a shared login serves anyone: where a login is
/// stored decides whom it serves. A policy narrows that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginPolicy {
    /// Tiers tried in order.
    #[serde(default = "default_login_tiers")]
    pub tiers: Vec<CredentialTier>,
    /// Who the agent's shared login may pay for.
    #[serde(default)]
    pub shared_serves: SharedServes,
}

impl Default for LoginPolicy {
    fn default() -> Self {
        Self {
            tiers: default_login_tiers(),
            shared_serves: SharedServes::default(),
        }
    }
}

fn default_login_tiers() -> Vec<CredentialTier> {
    vec![
        CredentialTier::AgentUser,
        CredentialTier::GlobalUser,
        CredentialTier::AgentShared,
        CredentialTier::SystemShared,
    ]
}

impl LoginPolicy {
    pub fn validate(&self) -> eyre::Result<()> {
        if self.tiers.is_empty() {
            return Err(eyre::eyre!(
                "[llm.logins] tiers must name at least one tier"
            ));
        }
        let mut seen = std::collections::HashSet::new();
        match self.tiers.iter().find(|tier| !seen.insert(**tier)) {
            Some(tier) => Err(eyre::eyre!(
                "[llm.logins] tiers lists {} more than once",
                tier.as_str()
            )),
            None => Ok(()),
        }
    }
}

/// Install-wide subscription settings (`[subscription]`), read at boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionConfig {
    /// Allows the direct ChatGPT provider. Off, a `chatgpt` route refuses
    /// every call with a named reason.
    #[serde(default)]
    pub chatgpt_direct: bool,
    /// The identity whose plan pays for the install's own work, and for an
    /// agent's background work when the agent has no single owner.
    #[serde(default)]
    pub operator: Option<uuid::Uuid>,
    /// One-shot CLI completions (`claude-code` and `codex` background
    /// calls) the hub runs at once; `0` is unbounded.
    #[serde(default = "default_max_concurrent_completions")]
    pub max_concurrent_completions: u32,
    /// Agent turns on a CLI route (`claude-code` or `codex`) generating at
    /// once; a turn waiting on a tool call or an approval holds none. `0` is
    /// unbounded.
    #[serde(default = "default_max_concurrent_cli_turns")]
    pub max_concurrent_cli_turns: u32,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            chatgpt_direct: false,
            operator: None,
            max_concurrent_completions: default_max_concurrent_completions(),
            max_concurrent_cli_turns: default_max_concurrent_cli_turns(),
        }
    }
}

fn default_max_concurrent_completions() -> u32 {
    4
}

fn default_max_concurrent_cli_turns() -> u32 {
    4
}

/// Retry policy for transient LLM errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum retries before failing over to the next model.
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
    /// Initial backoff duration in milliseconds.
    #[serde(default = "default_retry_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff multiplier (exponential).
    #[serde(default = "default_retry_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Maximum backoff duration in milliseconds.
    #[serde(default = "default_retry_max_backoff_ms")]
    pub max_backoff_ms: u64,
    /// Retry `Overloaded` (529) on the same model with backoff before failing
    /// over, instead of failing over immediately. Keeps transient provider
    /// overload (e.g. Bedrock) from spilling onto a metered fallback account.
    #[serde(default = "default_retry_overloaded")]
    pub retry_overloaded: bool,
    /// Retry `RateLimited` (429) on the same model when the provider's
    /// retry-after hint is short, instead of failing over immediately.
    /// Built for per-minute token quotas (e.g. Cerebras TPM): the window
    /// resets in seconds, while failing over swaps to a different model
    /// mid-conversation. Hint-less 429s still fail over.
    #[serde(default = "default_retry_rate_limited")]
    pub retry_rate_limited: bool,
    /// Longest retry-after hint (seconds) worth sleeping on before retrying
    /// the same model; longer hints fail over. Default 70 covers a full
    /// per-minute quota window plus jitter.
    #[serde(default = "default_max_rate_limit_wait_secs")]
    pub max_rate_limit_wait_secs: u64,
    /// Max seconds to wait for the first stream event of a streaming call.
    /// Fires before anything was surfaced, so the failover chain silently
    /// moves to the next model. `0` disables.
    #[serde(default = "default_first_token_timeout_secs")]
    pub first_token_timeout_secs: u64,
    /// Max seconds between stream events once streaming has begun. Fires
    /// after partial content was surfaced, so the turn aborts with a clear
    /// error instead of failing over. `0` disables.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,
    /// Max seconds for a whole non-streaming request. `0` disables.
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Per-model timeout overrides, keyed by exact model id or a substring
    /// of it — keep keys specific enough to hit only what you mean
    /// (`"gemini-3.1"` matches every model containing it). When several
    /// substring keys match, the longest wins.
    #[serde(default)]
    pub model_timeouts: HashMap<String, ModelTimeoutOverride>,
}

/// Per-model override entry for the `[llm.retry]` watchdog timeouts; unset
/// fields inherit the global value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTimeoutOverride {
    #[serde(default)]
    pub first_token_timeout_secs: Option<u64>,
    #[serde(default)]
    pub stall_timeout_secs: Option<u64>,
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_retry_max_retries(),
            initial_backoff_ms: default_retry_initial_backoff_ms(),
            backoff_multiplier: default_retry_backoff_multiplier(),
            max_backoff_ms: default_retry_max_backoff_ms(),
            retry_overloaded: default_retry_overloaded(),
            retry_rate_limited: default_retry_rate_limited(),
            max_rate_limit_wait_secs: default_max_rate_limit_wait_secs(),
            first_token_timeout_secs: default_first_token_timeout_secs(),
            stall_timeout_secs: default_stall_timeout_secs(),
            request_timeout_secs: default_request_timeout_secs(),
            model_timeouts: HashMap::new(),
        }
    }
}

fn default_retry_max_retries() -> u32 {
    3
}
fn default_retry_initial_backoff_ms() -> u64 {
    1000
}
fn default_retry_backoff_multiplier() -> f64 {
    2.0
}
fn default_retry_max_backoff_ms() -> u64 {
    30_000
}
fn default_retry_overloaded() -> bool {
    false
}
fn default_retry_rate_limited() -> bool {
    false
}
fn default_max_rate_limit_wait_secs() -> u64 {
    70
}
fn default_first_token_timeout_secs() -> u64 {
    45
}
fn default_stall_timeout_secs() -> u64 {
    180
}
fn default_request_timeout_secs() -> u64 {
    300
}

/// Proactive token-per-minute pacing for the provider pool. Requests are
/// admitted against a rolling 60s window of estimated tokens so the hub
/// paces or fails over BEFORE burning a request into a 429 — whose
/// retry-after is the full punitive window reset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Provider tokens-per-minute quota (e.g. Cerebras PAYG GLM-4.7: 500000).
    /// Cached tokens count toward provider TPM, so estimates use the full
    /// prompt.
    #[serde(default = "default_pacing_tpm_limit")]
    pub tpm_limit: u32,
    /// Fraction of the quota to admit against; the slack absorbs estimate
    /// error (the reactive 429 retry remains as backstop).
    #[serde(default = "default_pacing_headroom")]
    pub headroom: f64,
    /// Pace (sleep) up to this many seconds; longer projected waits fail
    /// over to the next model in the chain instead.
    #[serde(default = "default_pacing_max_wait_secs")]
    pub max_wait_secs: u64,
    /// Tokens reserved per request for the completion.
    #[serde(default = "default_pacing_output_allowance")]
    pub output_allowance: u32,
}

impl Default for PacingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tpm_limit: default_pacing_tpm_limit(),
            headroom: default_pacing_headroom(),
            max_wait_secs: default_pacing_max_wait_secs(),
            output_allowance: default_pacing_output_allowance(),
        }
    }
}

fn default_pacing_tpm_limit() -> u32 {
    500_000
}
fn default_pacing_headroom() -> f64 {
    0.85
}
fn default_pacing_max_wait_secs() -> u64 {
    30
}
fn default_pacing_output_allowance() -> u32 {
    1024
}

/// Model health tracking configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHealthConfig {
    /// Number of failures within `window_secs` to mark a model degraded.
    #[serde(default = "default_health_failure_threshold")]
    pub failure_threshold: u32,
    /// Time window in seconds for failure counting.
    #[serde(default = "default_health_window_secs")]
    pub window_secs: u64,
    /// Seconds before retrying a degraded model.
    #[serde(default = "default_health_recovery_secs")]
    pub recovery_secs: u64,
}

impl Default for ModelHealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_health_failure_threshold(),
            window_secs: default_health_window_secs(),
            recovery_secs: default_health_recovery_secs(),
        }
    }
}

fn default_health_failure_threshold() -> u32 {
    3
}
fn default_health_window_secs() -> u64 {
    300
}
fn default_health_recovery_secs() -> u64 {
    120
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            api_key: None,
            default_model: default_model(),
            max_tokens: default_max_tokens(),
            max_rounds: default_max_rounds(),
            interactive_max_rounds: default_interactive_max_rounds(),
            failover: Vec::new(),
            aliases: std::collections::HashMap::new(),
            retry: None,
            pacing: PacingConfig::default(),
            health: None,
            accounts: Vec::new(),
            thinking_budget_tokens: None,
            model_max_tokens: std::collections::HashMap::new(),
            fetch_pricing: false,
            zai_subscription: false,
            vision: None,
            transcription: None,
            image: None,
            video: None,
            speech: None,
            logins: LoginPolicy::default(),
        }
    }
}

impl LlmConfig {
    /// Check if failover/aliases are configured.
    pub fn has_failover_config(&self) -> bool {
        !self.failover.is_empty() || !self.aliases.is_empty()
    }
}

/// Session lifecycle management configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLifecycleConfig {
    /// Reset all sessions daily at this hour (0-23, in user timezone).
    /// Set to `None` to disable daily resets.
    #[serde(default)]
    pub daily_reset_hour: Option<u32>,
    /// Expire idle sessions after this many minutes of inactivity.
    /// Set to 0 or `None` to disable.
    #[serde(default)]
    pub idle_expiry_minutes: Option<u32>,
    /// Archive expired sessions to disk before clearing.
    #[serde(default = "default_archive_expired")]
    pub archive_expired: bool,
    /// How often to check for expired sessions (in seconds).
    #[serde(default = "default_lifecycle_check_interval")]
    pub check_interval_secs: u64,
}

impl Default for SessionLifecycleConfig {
    fn default() -> Self {
        Self {
            daily_reset_hour: None,
            idle_expiry_minutes: None,
            archive_expired: true,
            check_interval_secs: default_lifecycle_check_interval(),
        }
    }
}

impl SessionLifecycleConfig {
    /// Check if any lifecycle management is enabled.
    pub fn is_enabled(&self) -> bool {
        self.daily_reset_hour.is_some() || self.idle_expiry_minutes.is_some_and(|m| m > 0)
    }
}

fn default_archive_expired() -> bool {
    true
}

// ─── Session Reset Policies ──────────────────────────────────────────────────

/// Per-type and per-channel session reset policies.
///
/// These policies layer on top of `SessionLifecycleConfig` to allow fine-grained
/// control. The resolution order is:
///
/// 1. Channel-specific override (e.g., `[session_policies.channels.slack]`)
/// 2. Context-type default (e.g., `[session_policies.types.dm]`)
/// 3. Global `session_lifecycle` config
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionPoliciesConfig {
    /// Policies keyed by session context type (dm, group, thread).
    #[serde(default)]
    pub types: HashMap<SessionContextType, ResetPolicy>,
    /// Per-channel overrides keyed by channel name (e.g., "slack", "cli", "gateway").
    #[serde(default)]
    pub channels: HashMap<String, ResetPolicy>,
}

/// A reset policy that can be applied to a session type or channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResetPolicy {
    /// Whether daily resets apply. If `None`, inherits from global config.
    #[serde(default)]
    pub daily_reset: Option<bool>,
    /// Override the reset hour (0-23, in user timezone). If `None`, inherits from global.
    #[serde(default)]
    pub reset_hour: Option<u32>,
    /// Override idle expiry minutes. If `None`, inherits from global.
    /// Set to `0` to explicitly disable idle expiry for this scope.
    #[serde(default)]
    pub idle_expiry_minutes: Option<u32>,
}

/// Resolved reset policy for a specific session, after layering
/// channel overrides → type defaults → global config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedResetPolicy {
    /// Whether daily resets should apply.
    pub daily_reset: bool,
    /// The hour at which the daily reset triggers (0-23, user timezone).
    pub reset_hour: Option<u32>,
    /// Idle expiry in minutes (0 = disabled).
    pub idle_expiry_minutes: u32,
}

impl SessionPoliciesConfig {
    /// Resolve the effective reset policy for a session given its channel name
    /// and optional context type, falling back to the global lifecycle config.
    pub fn resolve(
        &self,
        channel: &str,
        context_type: Option<&SessionContextType>,
        global: &SessionLifecycleConfig,
    ) -> ResolvedResetPolicy {
        // Start from global defaults
        let global_daily = global.daily_reset_hour.is_some();
        let global_hour = global.daily_reset_hour;
        let global_idle = global.idle_expiry_minutes.unwrap_or(0);

        // Layer 1: type defaults (if context type is known)
        let type_policy = context_type.and_then(|ct| self.types.get(ct));
        let after_type_daily = type_policy
            .and_then(|p| p.daily_reset)
            .unwrap_or(global_daily);
        let after_type_hour = type_policy.and_then(|p| p.reset_hour).or(global_hour);
        let after_type_idle = type_policy
            .and_then(|p| p.idle_expiry_minutes)
            .unwrap_or(global_idle);

        // Layer 2: channel overrides (highest priority)
        let channel_policy = self.channels.get(channel);
        let final_daily = channel_policy
            .and_then(|p| p.daily_reset)
            .unwrap_or(after_type_daily);
        let final_hour = channel_policy
            .and_then(|p| p.reset_hour)
            .or(after_type_hour);
        let final_idle = channel_policy
            .and_then(|p| p.idle_expiry_minutes)
            .unwrap_or(after_type_idle);

        ResolvedResetPolicy {
            daily_reset: final_daily,
            reset_hour: if final_daily { final_hour } else { None },
            idle_expiry_minutes: final_idle,
        }
    }
}

/// Heartbeat system configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatSettings {
    /// Whether heartbeat is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Interval in seconds between heartbeats.
    #[serde(default = "default_heartbeat_interval")]
    pub interval_secs: u64,
    /// Active hours start (HH:MM format, in user timezone).
    #[serde(default = "default_active_start")]
    pub active_start: String,
    /// Active hours end (HH:MM format, in user timezone).
    #[serde(default = "default_active_end")]
    pub active_end: String,
    /// Channels to deliver heartbeat output to (e.g. ["slack-general", "ws"]).
    /// Empty = deliver to internal session only.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Delivery mode: "all" delivers every heartbeat response,
    /// "actionable_only" suppresses HEARTBEAT_OK responses.
    #[serde(default = "default_deliver_mode")]
    pub deliver: HeartbeatDeliverMode,
    /// Number of recent heartbeat results to keep in history.
    #[serde(default = "default_heartbeat_history_size")]
    pub history_size: usize,
    /// Custom heartbeat prompt (overrides default).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Maximum checks to run per heartbeat cycle.
    #[serde(default = "default_max_checks_per_cycle")]
    pub max_checks_per_cycle: usize,
    /// Deduplication window in seconds: suppress re-notification of same issue.
    #[serde(default = "default_dedup_window")]
    pub dedup_window_secs: u64,
    /// Whether to log heartbeat findings to daily notes.
    #[serde(default)]
    pub log_to_daily_notes: bool,
    /// Quiet hours start (HH:MM). Checks run but delivery is suppressed.
    #[serde(default)]
    pub quiet_start: Option<String>,
    /// Quiet hours end (HH:MM).
    #[serde(default)]
    pub quiet_end: Option<String>,
    /// Skip heartbeats during active user sessions.
    #[serde(default)]
    pub skip_during_active_sessions: bool,
    /// Backoff multiplier for consecutive failures.
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
    /// Maximum backoff in seconds.
    #[serde(default = "default_max_backoff")]
    pub max_backoff_secs: u64,
    /// The only tools a legacy (HEARTBEAT.md) heartbeat turn is offered, by
    /// name or namespace. Unset keeps the agent's full surface.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

/// How heartbeat responses are delivered to external channels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatDeliverMode {
    /// Deliver every heartbeat response.
    All,
    /// Only deliver if the response is actionable (suppress HEARTBEAT_OK).
    #[default]
    ActionableOnly,
}

impl Default for HeartbeatSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_heartbeat_interval(),
            active_start: default_active_start(),
            active_end: default_active_end(),
            channels: Vec::new(),
            deliver: HeartbeatDeliverMode::default(),
            history_size: default_heartbeat_history_size(),
            prompt: None,
            max_checks_per_cycle: default_max_checks_per_cycle(),
            dedup_window_secs: default_dedup_window(),
            log_to_daily_notes: false,
            quiet_start: None,
            quiet_end: None,
            skip_during_active_sessions: false,
            backoff_multiplier: default_backoff_multiplier(),
            max_backoff_secs: default_max_backoff(),
            tools: None,
        }
    }
}

fn default_heartbeat_interval() -> u64 {
    1800 // 30 minutes
}
fn default_active_start() -> String {
    "08:00".to_string()
}
fn default_active_end() -> String {
    "23:00".to_string()
}
fn default_deliver_mode() -> HeartbeatDeliverMode {
    HeartbeatDeliverMode::ActionableOnly
}
fn default_heartbeat_history_size() -> usize {
    20
}
fn default_max_checks_per_cycle() -> usize {
    4
}
fn default_dedup_window() -> u64 {
    3600
}
fn default_backoff_multiplier() -> f64 {
    2.0
}
fn default_max_backoff() -> u64 {
    7200
}

fn default_lifecycle_check_interval() -> u64 {
    300 // 5 minutes
}

/// Agent definition in config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Unique agent identifier (e.g., "allan", "architect").
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
    /// LLM model override (uses global [llm] default_model if not set).
    /// Deprecated: prefer `llm.default_model` for new configs.
    #[serde(default)]
    pub model: Option<String>,
    /// Path to workspace directory containing identity files (SOUL.md, etc.).
    /// Relative paths are resolved from the config file's parent directory.
    #[serde(default)]
    pub workspace: Option<String>,
    /// System prompt override (used instead of identity files if set).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Agent capabilities / skill tags.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Whether this is the default agent (used when no agent_id is specified).
    #[serde(default)]
    pub default: bool,
    /// Per-agent LLM overrides. Falls back to global `[llm]` for unset fields.
    #[serde(default)]
    pub llm: Option<AgentLlmConfig>,
    /// Per-agent vault overrides. Falls back to global `[vault]` for unset fields.
    #[serde(default)]
    pub vault: Option<AgentVaultConfig>,
    /// Per-agent compaction overrides. Falls back to global `[compaction]` for unset fields.
    #[serde(default)]
    pub compaction: Option<AgentCompactionConfig>,
    /// Per-agent memory overrides. Falls back to global `[memory]` for unset fields.
    #[serde(default)]
    pub memory: Option<AgentMemoryConfig>,
    /// Per-agent knowledge overrides. Falls back to global `[knowledge]` for unset fields.
    /// Agents with the same effective `root` share a single `KnowledgeService`.
    #[serde(default)]
    pub knowledge: Option<AgentKnowledgeConfig>,
    /// Per-agent access control policy (DW-149).
    #[serde(default)]
    pub access: AgentAccessConfig,
    /// Optional cross-surface scope filter. When present, every inbound user
    /// message is classified against the persona/custom prompt before reaching
    /// the agent — off-topic messages get a canned reply. Slack channel
    /// overrides (`channel_overrides.X.scope`) take precedence per-channel.
    #[serde(default)]
    pub scope: Option<ScopeFilterConfig>,
    /// Mapping of external user IDs to friendly profile filenames (DW-145).
    /// E.g., `{ "U12345" = "kings" }` makes `users/kings.md` resolve for that user.
    #[serde(default)]
    pub user_map: HashMap<String, String>,
    /// Owner of this agent (identity UUID or team UUID).
    #[serde(default)]
    pub owner_id: Option<String>,
    /// Whether the owner is a user or a team.
    #[serde(default)]
    pub owner_type: OwnerType,
    /// Per-agent advisor overrides. Falls back to global `[advisor]` for unset fields.
    #[serde(default)]
    pub advisor: Option<AgentAdvisorConfig>,
    /// Whether this agent may be invoked as another agent's advisor.
    /// When `false` (default), `source = "agent"` dispatches targeting this
    /// agent are refused with `AgentNotFound`. Setting this true is an
    /// affirmative opt-in: the agent's tools are filtered to a read-only
    /// set for the duration of an advisor dispatch.
    #[serde(default)]
    pub advisor_mode: bool,
}

/// Who owns an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    #[default]
    User,
    Team,
}

// ─── Access Control (DW-149) ────────────────────────────────────────────────

/// Access control mode for an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// Anyone can interact (default).
    #[default]
    Open,
    /// Only users in the `allow` list can interact.
    Allowlist,
    /// Everyone except users in the `block` list can interact.
    Blocklist,
}

/// Per-agent user access control configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentAccessConfig {
    /// Access mode.
    #[serde(default)]
    pub mode: AccessMode,
    /// Allowed user IDs (when mode = "allowlist").
    #[serde(default)]
    pub allow: Vec<String>,
    /// Blocked user IDs (when mode = "blocklist").
    #[serde(default)]
    pub block: Vec<String>,
    /// Message shown to denied users. If `None`, message is silently ignored.
    #[serde(default)]
    pub deny_message: Option<String>,
    /// Per-agent override for `[sentinel.external_users] allow`. `None` inherits
    /// the global default; `Some(true)` lets this agent serve external Slack
    /// senders (e.g. a support agent in a Connect channel).
    #[serde(default)]
    pub allow_external_users: Option<bool>,
}

/// Outcome of a cross-surface access check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Deny {
        message: Option<String>,
        reason: &'static str,
    },
}

impl AgentAccessConfig {
    /// Evaluate access for an inbound message. `identity_id` is the canonical
    /// workforce identity UUID (or `None` for anonymous); `surface_ids` are
    /// surface-prefixed fallbacks like `"slack:U123"` or `"ios_device:DEV-x"`.
    /// Empty candidates ⇒ allowlist denies, blocklist allows, open allows.
    pub fn evaluate(&self, identity_id: Option<&str>, surface_ids: &[&str]) -> AccessDecision {
        match self.mode {
            AccessMode::Open => AccessDecision::Allow,
            AccessMode::Allowlist => {
                if self.matches_any(identity_id, surface_ids, &self.allow) {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny {
                        message: self.deny_message.clone(),
                        reason: "allowlist_no_match",
                    }
                }
            }
            AccessMode::Blocklist => {
                if self.matches_any(identity_id, surface_ids, &self.block) {
                    AccessDecision::Deny {
                        message: self.deny_message.clone(),
                        reason: "blocklist_match",
                    }
                } else {
                    AccessDecision::Allow
                }
            }
        }
    }

    /// Normalize bare Slack IDs (`^[UWBT][A-Z0-9]+$`) in `allow`/`block` lists
    /// to `slack:`-prefixed form. Logs a deprecation warning for each
    /// migrated entry. Called by the gate at load and on hot-reload upsert.
    pub fn normalize_bare_slack_ids(&mut self, agent_id: &str) {
        normalize_list(&mut self.allow, agent_id, "allow");
        normalize_list(&mut self.block, agent_id, "block");
    }

    fn matches_any(
        &self,
        identity_id: Option<&str>,
        surface_ids: &[&str],
        list: &[String],
    ) -> bool {
        let mut candidates = identity_id.into_iter().chain(surface_ids.iter().copied());
        candidates.any(|c| list.iter().any(|entry| entry == c))
    }
}

fn normalize_list(list: &mut [String], agent_id: &str, field: &'static str) {
    for entry in list.iter_mut() {
        if let Some(prefixed) = bare_slack_id_to_prefixed(entry) {
            tracing::warn!(
                agent = %agent_id,
                field = %field,
                from = %entry,
                to = %prefixed,
                "Bare Slack ID in access list — auto-prefixing to \"slack:\". \
                 Update config to use the explicit prefix."
            );
            *entry = prefixed;
        }
    }
}

fn bare_slack_id_to_prefixed(entry: &str) -> Option<String> {
    if entry.contains(':') {
        return None;
    }
    let mut chars = entry.chars();
    let first = chars.next()?;
    if !matches!(first, 'U' | 'W' | 'B' | 'T') {
        return None;
    }
    if entry.len() < 9 {
        return None;
    }
    if !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return None;
    }
    Some(format!("slack:{entry}"))
}

// ─── DM Routing (DW-148) ───────────────────────────────────────────────────

/// DM routing configuration: maps user IDs to specific agents for direct messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DmRoutingConfig {
    /// User-to-agent mappings for DMs (e.g., `{ "U12345" = "architect" }`).
    #[serde(default)]
    pub user_routes: HashMap<String, String>,
    /// Default agent for DMs not explicitly routed.
    #[serde(default)]
    pub default_agent: Option<String>,
}

// ─── Daily Briefing (DW-168) ────────────────────────────────────────────────

/// Daily briefing configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BriefingConfig {
    /// Whether the daily briefing is enabled (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Cron expression for the briefing schedule (default: weekdays at 9 AM).
    #[serde(default = "default_briefing_cron")]
    pub cron: String,
    /// Timezone for the cron schedule (IANA name). Falls back to workspace timezone.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Slack channel ID to deliver the briefing to.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Slack thread timestamp for threaded delivery.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Custom briefing prompt. If None, uses the default prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model override for the briefing agent turn.
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for BriefingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: default_briefing_cron(),
            timezone: None,
            channel_id: None,
            thread_id: None,
            prompt: None,
            model: None,
        }
    }
}

fn default_briefing_cron() -> String {
    "0 9 * * 1-5".to_string()
}

// ─── Agent Communications (DW-219) ──────────────────────────────────────────

/// Agent communications oversight configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCommsSettings {
    /// Oversight mode for agent-to-agent messages.
    #[serde(default)]
    pub oversight_mode: String, // "auto", "require_approval", "notify"
    /// Agent pairs that are always auto-approved (format: "from:to").
    #[serde(default)]
    pub auto_approve_pairs: Vec<String>,
}

/// Per-agent compaction configuration overrides.
///
/// All fields are optional — unset fields fall back to the global `[compaction]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentCompactionConfig {
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub compaction_threshold: Option<f32>,
    #[serde(default)]
    pub keep_recent_messages: Option<usize>,
    #[serde(default)]
    pub enable_memory_flush: Option<bool>,
    #[serde(default)]
    pub prune_tool_outputs: Option<bool>,
    #[serde(default)]
    pub prune_tool_output_max_chars: Option<usize>,
    #[serde(default)]
    pub prune_tool_input_max_chars: Option<usize>,
    #[serde(default)]
    pub max_compaction_passes: Option<u32>,
    #[serde(default)]
    pub min_messages_to_compact: Option<usize>,
    #[serde(default)]
    pub urgent_compaction_threshold: Option<f32>,
    #[serde(default)]
    pub soft_eviction_threshold: Option<f32>,
    #[serde(default)]
    pub compaction_model: Option<String>,
    /// Custom system prompt for memory flush.
    #[serde(default)]
    pub flush_prompt: Option<String>,
    /// Custom system prompt for conversation summarisation.
    #[serde(default)]
    pub summary_prompt: Option<String>,
    #[serde(default)]
    pub stale_rollover_hours: Option<u64>,
    #[serde(default)]
    pub voice_rollover_max_messages: Option<usize>,
    #[serde(default)]
    pub chat_rollover_max_messages: Option<usize>,
}

/// Per-agent memory configuration overrides.
///
/// All fields are optional — unset fields fall back to the global `[memory]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMemoryConfig {
    #[serde(default)]
    pub passive: Option<bool>,
    #[serde(default)]
    pub passive_model: Option<String>,
    #[serde(default)]
    pub passive_min_turn_tokens: Option<u32>,
    #[serde(default)]
    pub passive_prompt: Option<String>,
    #[serde(default)]
    pub resume_context: Option<bool>,
    #[serde(default)]
    pub relationship: Option<bool>,
    #[serde(default)]
    pub relationship_min_turn_tokens: Option<u32>,
    #[serde(default)]
    pub consolidation: Option<bool>,
    #[serde(default)]
    pub consolidation_interval_secs: Option<u64>,
    #[serde(default)]
    pub consolidation_lookback_secs: Option<u64>,
    #[serde(default)]
    pub consolidation_model: Option<String>,
    #[serde(default)]
    pub consolidation_source: Option<CurationSource>,
    #[serde(default)]
    pub consolidation_cc_failover: Option<bool>,
    #[serde(default)]
    pub lesson_extract: Option<bool>,
    #[serde(default)]
    pub lesson_extract_model: Option<String>,
    #[serde(default)]
    pub lesson_extract_prompt: Option<String>,
    #[serde(default)]
    pub lesson_inject: Option<bool>,
    #[serde(default)]
    pub lesson_inject_limit: Option<usize>,
    #[serde(default)]
    pub lesson_inject_min_confidence: Option<f32>,
    #[serde(default)]
    pub lesson_inject_min_importance: Option<f32>,
    #[serde(default)]
    pub lesson_inject_min_score: Option<f32>,
    #[serde(default)]
    pub lesson_inject_min_top_score: Option<f32>,
    #[serde(default)]
    pub recall_min_certainty: Option<f32>,
    #[serde(default)]
    pub recall_recency_boost: Option<f32>,
    #[serde(default)]
    pub recall_recency_half_life_hours: Option<f32>,
    #[serde(default)]
    pub lesson_recall_alpha: Option<f32>,
    #[serde(default)]
    pub agent_recall_scope: Option<AgentRecallScope>,
    #[serde(default)]
    pub lesson_decay: Option<bool>,
    #[serde(default)]
    pub lesson_decay_idle_days: Option<i64>,
    #[serde(default)]
    pub lesson_decay_step: Option<f32>,
    #[serde(default)]
    pub lesson_decay_prune_floor: Option<f32>,
}

/// Per-agent knowledge configuration overrides.
///
/// All fields are optional — unset fields fall back to the global `[knowledge]` section.
/// Agents with the same effective `root` share a single `KnowledgeService` instance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentKnowledgeConfig {
    /// Override the root directory to index for this agent.
    #[serde(default)]
    pub root: Option<String>,
    /// Override the data directory for this agent's knowledge DB.
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Whether knowledge is enabled for this agent (defaults to global).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Additional exclude patterns (replaces global if set).
    #[serde(default)]
    pub exclude_patterns: Option<Vec<String>>,
}

impl AgentConfig {
    /// Resolve the effective knowledge config for this agent by merging
    /// per-agent overrides onto the global config. Unset fields inherit from global.
    pub fn effective_knowledge(&self, global: &KnowledgeConfig) -> KnowledgeConfig {
        match &self.knowledge {
            None => global.clone(),
            Some(agent) => KnowledgeConfig {
                enabled: agent.enabled.unwrap_or(global.enabled),
                root: agent.root.clone().or_else(|| global.root.clone()),
                data_dir: agent.data_dir.clone().or_else(|| global.data_dir.clone()),
                index_on_startup: global.index_on_startup,
                parse_concurrency: global.parse_concurrency,
                summary_concurrency: global.summary_concurrency,
                generate_summaries: global.generate_summaries,
                exclude_patterns: agent
                    .exclude_patterns
                    .clone()
                    .unwrap_or_else(|| global.exclude_patterns.clone()),
                max_file_size: global.max_file_size,
                maintenance_interval_secs: global.maintenance_interval_secs,
                index_timeout_secs: global.index_timeout_secs,
                summariser: global.summariser.clone(),
                vector_backend: global.vector_backend.clone(),
                vector_standby: global.vector_standby.clone(),
                vector_backfill_per_sec: global.vector_backfill_per_sec,
                vector_search_timeout_ms: global.vector_search_timeout_ms,
                vector_reconcile_secs: global.vector_reconcile_secs,
            },
        }
    }

    /// Resolve the effective compaction config for this agent by merging
    /// per-agent overrides onto the global config. Unset fields inherit from global.
    pub fn effective_compaction(&self, global: &CompactionConfig) -> CompactionConfig {
        match &self.compaction {
            None => global.clone(),
            Some(agent) => CompactionConfig {
                max_context_tokens: agent
                    .max_context_tokens
                    .unwrap_or(global.max_context_tokens),
                compaction_threshold: agent
                    .compaction_threshold
                    .unwrap_or(global.compaction_threshold),
                keep_recent_messages: agent
                    .keep_recent_messages
                    .unwrap_or(global.keep_recent_messages),
                enable_memory_flush: agent
                    .enable_memory_flush
                    .unwrap_or(global.enable_memory_flush),
                prune_tool_outputs: agent
                    .prune_tool_outputs
                    .unwrap_or(global.prune_tool_outputs),
                prune_tool_output_max_chars: agent
                    .prune_tool_output_max_chars
                    .unwrap_or(global.prune_tool_output_max_chars),
                prune_tool_input_max_chars: agent
                    .prune_tool_input_max_chars
                    .unwrap_or(global.prune_tool_input_max_chars),
                max_compaction_passes: agent
                    .max_compaction_passes
                    .unwrap_or(global.max_compaction_passes),
                min_messages_to_compact: agent
                    .min_messages_to_compact
                    .unwrap_or(global.min_messages_to_compact),
                urgent_compaction_threshold: agent
                    .urgent_compaction_threshold
                    .unwrap_or(global.urgent_compaction_threshold),
                soft_eviction_threshold: agent
                    .soft_eviction_threshold
                    .unwrap_or(global.soft_eviction_threshold),
                compaction_model: agent
                    .compaction_model
                    .clone()
                    .or_else(|| global.compaction_model.clone()),
                flush_prompt: agent
                    .flush_prompt
                    .clone()
                    .or_else(|| global.flush_prompt.clone()),
                summary_prompt: agent
                    .summary_prompt
                    .clone()
                    .or_else(|| global.summary_prompt.clone()),
                rollover_warning_ratio: global.rollover_warning_ratio,
                rollover_keep_recent: global.rollover_keep_recent,
                stale_rollover_hours: agent
                    .stale_rollover_hours
                    .unwrap_or(global.stale_rollover_hours),
                voice_rollover_max_messages: agent
                    .voice_rollover_max_messages
                    .unwrap_or(global.voice_rollover_max_messages),
                chat_rollover_max_messages: agent
                    .chat_rollover_max_messages
                    .unwrap_or(global.chat_rollover_max_messages),
            },
        }
    }

    /// Resolve the effective memory config for this agent by merging
    /// per-agent overrides onto the global config. Unset fields inherit from global.
    pub fn effective_memory(&self, global: &MemoryConfig) -> MemoryConfig {
        match &self.memory {
            None => global.clone(),
            Some(agent) => MemoryConfig {
                backend: global.backend.clone(),
                db_path: global.db_path.clone(),
                embedding_provider: global.embedding_provider.clone(),
                embedding_model: global.embedding_model.clone(),
                embedding_api_url: global.embedding_api_url.clone(),
                embedding_api_key: global.embedding_api_key.clone(),
                passive: agent.passive.unwrap_or(global.passive),
                passive_model: agent
                    .passive_model
                    .clone()
                    .or_else(|| global.passive_model.clone()),
                passive_min_turn_tokens: agent
                    .passive_min_turn_tokens
                    .unwrap_or(global.passive_min_turn_tokens),
                passive_prompt: agent
                    .passive_prompt
                    .clone()
                    .or_else(|| global.passive_prompt.clone()),
                resume_context: agent.resume_context.unwrap_or(global.resume_context),
                relationship: agent.relationship.unwrap_or(global.relationship),
                relationship_min_turn_tokens: agent
                    .relationship_min_turn_tokens
                    .unwrap_or(global.relationship_min_turn_tokens),
                consolidation: agent.consolidation.unwrap_or(global.consolidation),
                consolidation_interval_secs: agent
                    .consolidation_interval_secs
                    .unwrap_or(global.consolidation_interval_secs),
                consolidation_lookback_secs: agent
                    .consolidation_lookback_secs
                    .unwrap_or(global.consolidation_lookback_secs),
                consolidation_model: agent
                    .consolidation_model
                    .clone()
                    .or_else(|| global.consolidation_model.clone()),
                consolidation_source: agent
                    .consolidation_source
                    .unwrap_or(global.consolidation_source),
                consolidation_cc_failover: agent
                    .consolidation_cc_failover
                    .unwrap_or(global.consolidation_cc_failover),
                lesson_extract: agent.lesson_extract.unwrap_or(global.lesson_extract),
                lesson_extract_model: agent
                    .lesson_extract_model
                    .clone()
                    .or_else(|| global.lesson_extract_model.clone()),
                lesson_extract_prompt: agent
                    .lesson_extract_prompt
                    .clone()
                    .or_else(|| global.lesson_extract_prompt.clone()),
                lesson_inject: agent.lesson_inject.unwrap_or(global.lesson_inject),
                lesson_inject_limit: agent
                    .lesson_inject_limit
                    .unwrap_or(global.lesson_inject_limit),
                lesson_inject_min_confidence: agent
                    .lesson_inject_min_confidence
                    .unwrap_or(global.lesson_inject_min_confidence),
                lesson_inject_min_importance: agent
                    .lesson_inject_min_importance
                    .unwrap_or(global.lesson_inject_min_importance),
                lesson_inject_min_score: agent
                    .lesson_inject_min_score
                    .unwrap_or(global.lesson_inject_min_score),
                lesson_inject_min_top_score: agent
                    .lesson_inject_min_top_score
                    .unwrap_or(global.lesson_inject_min_top_score),
                recall_min_certainty: agent.recall_min_certainty.or(global.recall_min_certainty),
                recall_recency_boost: agent
                    .recall_recency_boost
                    .unwrap_or(global.recall_recency_boost),
                recall_recency_half_life_hours: agent
                    .recall_recency_half_life_hours
                    .unwrap_or(global.recall_recency_half_life_hours),
                lesson_recall_alpha: agent.lesson_recall_alpha.or(global.lesson_recall_alpha),
                agent_recall_scope: agent
                    .agent_recall_scope
                    .unwrap_or(global.agent_recall_scope),
                lesson_decay: agent.lesson_decay.unwrap_or(global.lesson_decay),
                lesson_decay_idle_days: agent
                    .lesson_decay_idle_days
                    .unwrap_or(global.lesson_decay_idle_days),
                lesson_decay_step: agent.lesson_decay_step.unwrap_or(global.lesson_decay_step),
                lesson_decay_prune_floor: agent
                    .lesson_decay_prune_floor
                    .unwrap_or(global.lesson_decay_prune_floor),
                derived_figures: global.derived_figures.clone(),
            },
        }
    }

    /// Resolve the effective advisor config for this agent by merging
    /// per-agent overrides onto the global config. Unset fields inherit from global.
    pub fn effective_advisor(&self, global: &AdvisorConfig) -> AdvisorConfig {
        match &self.advisor {
            None => global.clone(),
            Some(agent) => AdvisorConfig {
                enabled: agent.enabled.unwrap_or(global.enabled),
                source: agent.source.unwrap_or(global.source),
                payload: agent.payload.unwrap_or(global.payload),
                payload_max_tokens: agent
                    .payload_max_tokens
                    .unwrap_or(global.payload_max_tokens),
                per_turn_invocation_limit: agent
                    .per_turn_invocation_limit
                    .unwrap_or(global.per_turn_invocation_limit),
                model: agent.model.clone().or_else(|| global.model.clone()),
                cc_allow_failover_to_models: agent
                    .cc_allow_failover_to_models
                    .unwrap_or(global.cc_allow_failover_to_models),
                cc_fallback_model: agent
                    .cc_fallback_model
                    .clone()
                    .or_else(|| global.cc_fallback_model.clone()),
                max_rounds: agent.max_rounds.unwrap_or(global.max_rounds),
                cc_effort: agent.cc_effort.clone().or_else(|| global.cc_effort.clone()),
                agent_id: agent.agent_id.clone().or_else(|| global.agent_id.clone()),
                system_prompt_suffix: agent
                    .system_prompt_suffix
                    .clone()
                    .or_else(|| global.system_prompt_suffix.clone()),
                fanout: agent
                    .fanout
                    .clone()
                    .unwrap_or_else(|| global.fanout.clone()),
            },
        }
    }

    /// Resolve the effective LLM config for this agent by merging per-agent
    /// overrides onto the global config. Unset per-agent fields inherit from global.
    pub fn effective_llm(&self, global: &LlmConfig) -> LlmConfig {
        match &self.llm {
            None => {
                // No per-agent LLM section — use global, with legacy model override
                let mut llm = global.clone();
                if let Some(ref m) = self.model {
                    llm.default_model = m.clone();
                }
                llm
            }
            Some(agent) => LlmConfig {
                provider: agent
                    .provider
                    .clone()
                    .unwrap_or_else(|| global.provider.clone()),
                api_key: agent.api_key.clone().or_else(|| global.api_key.clone()),
                default_model: agent
                    .default_model
                    .clone()
                    .or_else(|| self.model.clone())
                    .unwrap_or_else(|| global.default_model.clone()),
                max_tokens: agent.max_tokens.unwrap_or(global.max_tokens),
                max_rounds: agent.max_rounds.unwrap_or(global.max_rounds),
                interactive_max_rounds: agent
                    .interactive_max_rounds
                    .unwrap_or(global.interactive_max_rounds),
                failover: agent
                    .failover
                    .clone()
                    .unwrap_or_else(|| global.failover.clone()),
                aliases: agent
                    .aliases
                    .clone()
                    .unwrap_or_else(|| global.aliases.clone()),
                retry: agent.retry.clone().or_else(|| global.retry.clone()),
                pacing: agent
                    .pacing
                    .clone()
                    .unwrap_or_else(|| global.pacing.clone()),
                health: agent.health.clone().or_else(|| global.health.clone()),
                accounts: agent
                    .accounts
                    .clone()
                    .unwrap_or_else(|| global.accounts.clone()),
                thinking_budget_tokens: agent
                    .thinking_budget_tokens
                    .or(global.thinking_budget_tokens),
                model_max_tokens: {
                    let mut map = global.model_max_tokens.clone();
                    if let Some(ref agent_map) = agent.model_max_tokens {
                        map.extend(agent_map.clone());
                    }
                    map
                },
                fetch_pricing: agent.fetch_pricing.unwrap_or(global.fetch_pricing),
                zai_subscription: agent.zai_subscription.unwrap_or(global.zai_subscription),
                vision: agent.vision.clone().or_else(|| global.vision.clone()),
                transcription: agent
                    .transcription
                    .clone()
                    .or_else(|| global.transcription.clone()),
                image: agent.image.clone().or_else(|| global.image.clone()),
                video: agent.video.clone().or_else(|| global.video.clone()),
                speech: agent.speech.clone().or_else(|| global.speech.clone()),
                logins: agent
                    .logins
                    .clone()
                    .unwrap_or_else(|| global.logins.clone()),
            },
        }
    }
}

/// Per-agent LLM configuration overrides.
///
/// All fields are optional — unset fields fall back to the global `[llm]` section.
///
/// ```toml
/// [[agents]]
/// id = "allan"
/// llm.provider = "anthropic"
/// llm.default_model = "claude-sonnet-4-20250514"
/// llm.failover = ["claude-haiku-4-5-20251001"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentLlmConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_rounds: Option<u32>,
    #[serde(default)]
    pub interactive_max_rounds: Option<u32>,
    #[serde(default)]
    pub failover: Option<Vec<String>>,
    #[serde(default)]
    pub aliases: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    /// Per-agent proactive TPM pacing override. `None` inherits the global
    /// `[llm.pacing]`. When present it FULLY replaces the global policy (not a
    /// field-level merge), so e.g. a high-quota direct-provider agent can set
    /// `[llm.pacing] enabled = false` to opt out of the fleet-wide cap.
    #[serde(default)]
    pub pacing: Option<PacingConfig>,
    #[serde(default)]
    pub health: Option<ModelHealthConfig>,
    #[serde(default)]
    pub accounts: Option<Vec<String>>,
    /// Adaptive model routing configuration.
    #[serde(default)]
    pub routing: Option<WorkspaceRoutingConfig>,
    /// Extended thinking budget in tokens. When set, the model can reason
    /// internally before producing output. `None` = inherit from global.
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// Per-task-type model preferences. Maps task type strings (e.g.
    /// "conversation", "coding") to model IDs. Used in model resolution
    /// between SQLite runtime preferences and the agent-wide override.
    ///
    /// ```toml
    /// [llm.model_preferences]
    /// conversation = "claude-sonnet-4-20250514"
    /// data_analysis = "claude-sonnet-4-20250514"
    /// ```
    #[serde(default)]
    pub model_preferences: Option<HashMap<String, String>>,
    /// Model for turns that arrive in voice mode (the iOS client's voice
    /// input). Spoken conversation is latency-bound, not depth-bound, so this
    /// lets one agent answer speech on a fast model while its typed turns stay
    /// on the default. Applies per *turn*, not per session: the same iOS
    /// session typing again goes back to the normal model.
    ///
    /// Takes the same forms as any other routed model spec — a tier
    /// (`"cheap"`), a provider tier (`"openrouter:anthropic:cheap"`,
    /// `"cerebras:qwen:standard"`), or a concrete id
    /// (`"claude-haiku-4-5-20251001"`).
    ///
    /// ```toml
    /// [llm]
    /// voice_model = "openrouter:anthropic:cheap"
    /// ```
    ///
    /// Beaten by an explicit per-session choice (`system.set_session_model`,
    /// `cmd: model`) and by a playbook step's model; beats task-type
    /// preferences and the agent-wide override. A spec routing to a provider
    /// the agent doesn't already talk to needs a hub restart to register that
    /// provider — same limitation as `model_preferences`.
    #[serde(default)]
    pub voice_model: Option<String>,
    /// Reasoning budget for voice turns, replacing `thinking_budget_tokens`
    /// for the duration of the turn. `0` means "don't think" — the fastest
    /// setting, and usually the right one, since hidden reasoning is pure
    /// dead air on a spoken reply.
    ///
    /// Providers apply it through their existing mappings: `minimal` /
    /// `low` / `medium` / `high` thinking levels on gemini-3.x, `none` and
    /// friends on OpenAI-shaped `reasoning.effort`, and a raw token budget on
    /// Anthropic (where anything under its 1024 floor disables thinking).
    ///
    /// Independent of [`Self::voice_model`] — set either, both, or neither.
    /// A background extraction that suppresses reasoning outright still wins
    /// over this, so a voice turn can't hand a classifier its chain of thought
    /// back.
    ///
    /// ```toml
    /// [llm]
    /// voice_model = "cheap"
    /// voice_thinking_budget_tokens = 0
    /// ```
    #[serde(default)]
    pub voice_thinking_budget_tokens: Option<u32>,
    /// Model tier equivalences across providers. Maps tier names ("powerful",
    /// "standard", "cheap") to lists of equivalent models. Used by adaptive
    /// routing and failover to pick the right model when switching providers.
    ///
    /// ```toml
    /// [llm.equivalences]
    /// powerful = ["claude-opus-4-6", "grok:grok-4", "gemini:gemini-2.5-pro"]
    /// standard = ["claude-sonnet-4-6", "grok:grok-3", "gemini:gemini-2.0-flash"]
    /// cheap = ["claude-haiku-4-5-20251001", "gemini:gemini-2.0-flash-lite"]
    /// ```
    #[serde(default)]
    pub equivalences: Option<HashMap<String, Vec<String>>>,
    /// Per-model max_tokens overrides. When the active model matches a key,
    /// this value is used instead of the global `max_tokens`.
    ///
    /// ```toml
    /// [llm.model_max_tokens]
    /// "claude-opus-4-6" = 128000
    /// ```
    #[serde(default)]
    pub model_max_tokens: Option<std::collections::HashMap<String, u32>>,
    /// Fetch model pricing from provider APIs at startup. `None` = inherit from global.
    #[serde(default)]
    pub fetch_pricing: Option<bool>,
    /// Use Z.ai Coding Plan subscription endpoint. `None` = inherit from global.
    #[serde(default)]
    pub zai_subscription: Option<bool>,
    /// Per-agent vision fallback override. `None` = inherit global `[llm.vision]`.
    /// Whole-block override: if set, none of the global vision fields leak through.
    #[serde(default)]
    pub vision: Option<LlmVisionConfig>,
    /// Per-agent transcription override. `None` = inherit global
    /// `[llm.transcription]`. Whole-block override semantics, same as vision.
    #[serde(default)]
    pub transcription: Option<LlmTranscriptionConfig>,
    /// Per-agent image-generation override. `None` = inherit global
    /// `[llm.image]`. Whole-block override semantics, same as vision.
    #[serde(default)]
    pub image: Option<LlmImageGenerationConfig>,
    /// Per-agent video-generation override. `None` = inherit global
    /// `[llm.video]`. Whole-block override semantics, same as image.
    #[serde(default)]
    pub video: Option<LlmVideoGenerationConfig>,
    /// Per-agent speech-synthesis override. `None` = inherit global
    /// `[llm.speech]`. Whole-block override semantics, same as image.
    #[serde(default)]
    pub speech: Option<LlmSpeechConfig>,
    /// Per-agent subscription login policy. `None` = inherit global
    /// `[llm.logins]`; a present value replaces it whole.
    #[serde(default)]
    pub logins: Option<LoginPolicy>,
}

/// Per-agent vault configuration overrides.
///
/// All fields are optional — unset fields fall back to the global `[vault]` section.
/// Vault path defaults to `{workspace}/vault.enc` (per-agent isolation).
///
/// ```toml
/// [[agents]]
/// id = "allan"
/// vault.enabled = true
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentVaultConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub key_file: Option<String>,
}

// ─── Workspace Config (per-agent runtime config, survives CI deploys) ────────

/// Per-agent runtime configuration stored in `{workspace}/config.toml`.
///
/// This is the second layer in the three-layer config system:
///   1. `workforce.toml` — CI-managed global defaults
///   2. `{workspace}/config.toml` — per-agent runtime config (this struct)
///   3. Vault — secrets only (API keys, tokens)
///
/// Agent tools (`system.set_model`, `system.set_failover_chain`, etc.) write
/// to this file instead of the vault, making the active config transparent and
/// inspectable.
///
/// All fields are optional — unset fields fall back to the agent section in
/// `workforce.toml`, then to the global defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentWorkspaceConfig {
    /// Per-agent LLM configuration overrides.
    #[serde(default)]
    pub llm: Option<AgentLlmConfig>,
    /// Per-agent compaction overrides.
    #[serde(default)]
    pub compaction: Option<AgentCompactionConfig>,
    /// Per-agent memory overrides (passive extraction, consolidation).
    #[serde(default)]
    pub memory: Option<AgentMemoryConfig>,
    /// Per-agent temporal perception configuration.
    #[serde(default)]
    pub temporal: Option<crate::temporal::TemporalConfig>,
    /// Per-agent advisor overrides.
    #[serde(default)]
    pub advisor: Option<AgentAdvisorConfig>,
    /// Per-agent access control override. When present, replaces the base
    /// `[access]` from the workforce config and is picked up on hot-reload.
    #[serde(default)]
    pub access: Option<AgentAccessConfig>,
    /// Per-agent scope filter override. When present, replaces the base
    /// `[scope]` from the workforce config and is picked up on hot-reload.
    #[serde(default)]
    pub scope: Option<ScopeFilterConfig>,
    /// Per-agent report/infographic theming (`generate_report` brand themes).
    #[serde(default)]
    pub report: Option<AgentReportConfig>,
    /// Per-agent override of `[worker_read_sources]`. When present, replaces the
    /// global value for this agent and is picked up on hot-reload — so an
    /// operator can turn worker read-on-demand on (or off) for one agent by
    /// editing its `config.toml`, no restart.
    #[serde(default)]
    pub worker_read_sources: Option<WorkerReadSourcesConfig>,
    /// Per-agent override of `[self_learning]`. When present, replaces the
    /// global value for this agent and is picked up on hot-reload — so an
    /// operator can turn self-learning on (or off) for one agent by editing
    /// its `config.toml`, no restart.
    #[serde(default)]
    pub self_learning: Option<SelfLearningConfig>,
    /// Per-agent override of `[claim_grounding]`. When present, replaces the
    /// global value for this agent and is picked up on hot-reload — so an
    /// operator can turn claim grounding on (or off) for one agent by editing
    /// its `config.toml`, no restart.
    #[serde(default)]
    pub claim_grounding: Option<ClaimGroundingConfig>,
    /// Per-agent override of `[code_context]`. When present, replaces the global
    /// value for this agent and is picked up on hot-reload — so an operator can
    /// turn answer-ready code-context injection off (or on) for one agent by
    /// editing its `config.toml`, no restart.
    #[serde(default)]
    pub code_context: Option<CodeContextConfig>,
    /// Per-agent override of the `[sessions]` digest knobs. When present,
    /// replaces the global digest settings for this agent; hot-reloaded.
    /// Deliberately NOT `[sessions] backend`, which stays global-only.
    #[serde(default)]
    pub session_digest: Option<SessionDigestConfig>,
    /// Per-agent `[tools]`: `[tools.programs]`, whether and how this agent
    /// runs Sift programs, `[tools.compile_gate]` and `[tools.refactor]`;
    /// hot-reloaded.
    #[serde(default)]
    pub tools: Option<AgentToolsConfig>,
    /// Per-agent `[know_how]`, field by field over the install's; hot-reloaded.
    #[serde(default)]
    pub know_how: Option<AgentKnowHowConfig>,
    /// Per-agent `[decision]`: whether this agent's decisions are recorded;
    /// hot-reloaded.
    #[serde(default)]
    pub decision: Option<AgentDecisionConfig>,
    /// Per-agent `[research]` (adversarial default, low-confidence bar);
    /// hot-reloaded.
    #[serde(default)]
    pub research: Option<AgentResearchConfig>,
    /// Per-agent `[handoff]` (verification default, correction rounds,
    /// verifier model); hot-reloaded.
    #[serde(default)]
    pub handoff: Option<AgentHandoffConfig>,
    /// Per-agent `[attachments]`, field by field over the install's;
    /// hot-reloaded.
    #[serde(default)]
    pub attachments: Option<AgentAttachmentsConfig>,
    /// Per-agent `[scheduler]`: whether this agent's recurring jobs may pin a
    /// session; hot-reloaded.
    #[serde(default)]
    pub scheduler: Option<AgentSchedulerConfig>,
    /// Per-agent `[evals]`: whether this agent is a test copy `workforce
    /// eval` may lay an arm's overrides over.
    #[serde(default)]
    pub evals: Option<AgentEvalsConfig>,
    /// This agent's own environments; hot-reloaded. Only a human changes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environments: Option<AgentEnvironmentsConfig>,
}

impl AgentWorkspaceConfig {
    /// This agent's own `[tools.programs]`, if its `config.toml` has one.
    pub fn programs(&self) -> Option<&AgentProgramsConfig> {
        self.tools.as_ref()?.programs.as_ref()
    }

    /// This agent's own `[tools.compile_gate]`, if its `config.toml` has one.
    pub fn compile_gate(&self) -> Option<&AgentCompileGateConfig> {
        self.tools.as_ref()?.compile_gate.as_ref()
    }

    /// This agent's own `[tools.refactor]`, if its `config.toml` has one.
    pub fn refactor(&self) -> Option<&AgentRefactorConfig> {
        self.tools.as_ref()?.refactor.as_ref()
    }

    /// This agent's own `[attachments.video]`, if its `config.toml` has one.
    pub fn video_frames(&self) -> Option<&AgentVideoFramesConfig> {
        self.attachments.as_ref()?.video.as_ref()
    }

    /// Whether this agent says of itself that it is a test copy.
    pub fn is_test_copy(&self) -> bool {
        self.evals.is_some_and(|evals| evals.copy)
    }

    /// `[know_how] source_agent` as it is set, whether or not it is read.
    pub fn know_how_source_set(&self) -> Option<&str> {
        self.know_how
            .as_ref()?
            .source_agent
            .as_deref()
            .map(str::trim)
            .filter(|source| !source.is_empty())
    }

    /// The agent whose know-how this one reads: `[know_how] source_agent`,
    /// honoured only on a test copy.
    pub fn know_how_source(&self) -> Option<&str> {
        self.know_how_source_set().filter(|_| self.is_test_copy())
    }
}

/// `[report]` in workspace `config.toml`: brand themes for `generate_report`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentReportConfig {
    /// Theme used when a report spec doesn't name one (a brand or built-in name).
    #[serde(default)]
    pub default_theme: Option<String>,
    /// Workspace default for emoji rendering. `Some(false)` strips emoji from
    /// every report unless a spec opts back in with `emoji: true`; `None`/`true`
    /// render them. A per-report `emoji` field always wins over this.
    #[serde(default)]
    pub emoji: Option<bool>,
    /// Brand themes by name, each inheriting a built-in base plus overrides.
    #[serde(default)]
    pub themes: std::collections::HashMap<String, BrandThemeConfig>,
}

/// A brand theme: inherits a built-in `base` and overrides only the brand
/// colours, fonts, and logo. Colours are `#rrggbb`; unset fields fall back to
/// the base. `logo` is a file path relative to the workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrandThemeConfig {
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub cover_bg: Option<String>,
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub secondary: Option<String>,
    #[serde(default)]
    pub accent: Option<String>,
    #[serde(default)]
    pub ink: Option<String>,
    #[serde(default)]
    pub paper: Option<String>,
    #[serde(default)]
    pub panel: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub hairline: Option<String>,
    #[serde(default)]
    pub on_dark: Option<String>,
    #[serde(default)]
    pub series: Option<Vec<String>>,
    #[serde(default)]
    pub sans: Option<String>,
    #[serde(default)]
    pub serif: Option<String>,
    #[serde(default)]
    pub logo: Option<String>,
}

/// Routing configuration for workspace config.toml.
/// Mirrors `workforce_agent::model_router::RoutingConfig` but with all fields
/// optional so partial overrides work.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceRoutingConfig {
    #[serde(default)]
    pub enable_adaptive_routing: Option<bool>,
    #[serde(default)]
    pub investigation_switch_round: Option<u32>,
    #[serde(default)]
    pub coding_switch_round: Option<u32>,
    #[serde(default)]
    pub read_only_threshold: Option<u32>,
    #[serde(default)]
    pub fast_round_threshold_ms: Option<u64>,
    #[serde(default)]
    pub escalate_on_haiku_failure: Option<bool>,
}

/// Whether an agent would load a workspace config, decided before the config
/// is written: every write of an agent's config.toml passes one.
pub trait AdmitWorkspace: Send + Sync {
    fn admit(&self, workspace: &AgentWorkspaceConfig) -> eyre::Result<()>;
}

impl AgentWorkspaceConfig {
    /// Load workspace config from `{workspace}/config.toml`.
    /// Returns `Ok(None)` if the file doesn't exist.
    pub fn load(workspace: &Path) -> eyre::Result<Option<Self>> {
        Ok(Self::load_with_text(workspace)?.map(|(config, _)| config))
    }

    /// [`Self::load`], with the file's text as it was read.
    pub fn load_with_text(workspace: &Path) -> eyre::Result<Option<(Self, String)>> {
        let config_path = workspace.join(crate::config_files::WORKSPACE_CONFIG);
        let Some(content) = crate::config_files::read(&config_path).map_err(|e| {
            eyre::eyre!(
                "Failed to read workspace config '{}': {}",
                config_path.display(),
                e
            )
        })?
        else {
            return Ok(None);
        };

        match Self::parse(&content) {
            Ok(config) => Ok(Some((config, content))),
            Err(e) => Err(eyre::eyre!(
                "Failed to parse workspace config '{}': {}",
                config_path.display(),
                e
            )),
        }
    }

    /// Parse a `config.toml` body exactly as [`Self::load`] reads the file.
    pub fn parse(content: &str) -> eyre::Result<Self> {
        let mut config: Self = toml::from_str(content)?;

        // Backwards compat: migrate root-level [routing] → [llm.routing]
        // Earlier versions stored routing at the top level instead of under [llm].
        if let Ok(raw) = content.parse::<toml::Table>() {
            if raw.contains_key("routing") {
                if let Ok(legacy) = raw["routing"].clone().try_into::<WorkspaceRoutingConfig>() {
                    let llm = config.llm.get_or_insert_with(Default::default);
                    if llm.routing.is_none() {
                        llm.routing = Some(legacy);
                    }
                }
            }
        }

        if let Some(environments) = &config.environments {
            environments.validate()?;
        }

        Ok(config)
    }

    /// Save workspace config to `{workspace}/config.toml` atomically.
    fn save(&self, workspace: &Path) -> eyre::Result<PathBuf> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| eyre::eyre!("Failed to serialize workspace config: {}", e))?;
        Self::write(
            workspace,
            &format!(
                "# Per-agent configuration — change it in the agent's settings.\n\
                 # Overrides global defaults from workforce.toml.\n\
                 # This file survives CI deploys.\n\
                 \n\
                 {content}"
            ),
        )
    }

    /// Replace `{workspace}/config.toml` with `content` atomically (see
    /// [`crate::config_files::replace`]). Callers hold [`Self::locked`].
    pub fn write(workspace: &Path, content: &str) -> eyre::Result<PathBuf> {
        let path = workspace.join(crate::config_files::WORKSPACE_CONFIG);
        crate::config_files::replace(&path, content.as_bytes()).map_err(|e| {
            eyre::eyre!(
                "Failed to write workspace config '{}': {}",
                path.display(),
                e
            )
        })?;
        Ok(path)
    }

    /// Run `write` holding the lock every write of `workspace`'s config.toml
    /// in this process holds from reading the file to replacing it, so no
    /// two writers interleave.
    pub fn locked<T>(workspace: &Path, write: impl FnOnce() -> T) -> T {
        static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
            LazyLock::new(Default::default);
        let lock = Arc::clone(
            LOCKS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(workspace.to_path_buf())
                .or_default(),
        );
        let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
        write()
    }

    /// The workspace config after `mutate`, starting from defaults when there
    /// is none.
    pub fn load_mutated(workspace: &Path, mutate: impl FnOnce(&mut Self)) -> eyre::Result<Self> {
        let mut config = Self::load(workspace)?.unwrap_or_default();
        mutate(&mut config);
        Ok(config)
    }

    /// Load the config, apply `mutate` and save it back, only if `admission`
    /// accepts the result: a refusal leaves the file untouched.
    pub fn update(
        workspace: &Path,
        admission: &dyn AdmitWorkspace,
        mutate: impl FnOnce(&mut Self),
    ) -> eyre::Result<()> {
        Self::locked(workspace, || {
            let config = Self::load_mutated(workspace, mutate)?;
            admission.admit(&config)?;
            config.save(workspace).map(drop)
        })
    }

    /// Merge this workspace config into an `AgentConfig`'s effective LLM config.
    /// Workspace config takes priority over the agent section from workforce.toml.
    pub fn merge_llm(&self, base: &LlmConfig) -> LlmConfig {
        match &self.llm {
            None => base.clone(),
            Some(ws) => LlmConfig {
                provider: ws.provider.clone().unwrap_or_else(|| base.provider.clone()),
                api_key: ws.api_key.clone().or_else(|| base.api_key.clone()),
                default_model: ws
                    .default_model
                    .clone()
                    .unwrap_or_else(|| base.default_model.clone()),
                max_tokens: ws.max_tokens.unwrap_or(base.max_tokens),
                max_rounds: ws.max_rounds.unwrap_or(base.max_rounds),
                interactive_max_rounds: ws
                    .interactive_max_rounds
                    .unwrap_or(base.interactive_max_rounds),
                failover: ws.failover.clone().unwrap_or_else(|| base.failover.clone()),
                aliases: ws.aliases.clone().unwrap_or_else(|| base.aliases.clone()),
                retry: ws.retry.clone().or_else(|| base.retry.clone()),
                pacing: ws.pacing.clone().unwrap_or_else(|| base.pacing.clone()),
                health: ws.health.clone().or_else(|| base.health.clone()),
                accounts: ws.accounts.clone().unwrap_or_else(|| base.accounts.clone()),
                thinking_budget_tokens: ws.thinking_budget_tokens.or(base.thinking_budget_tokens),
                model_max_tokens: {
                    let mut map = base.model_max_tokens.clone();
                    if let Some(ref ws_map) = ws.model_max_tokens {
                        map.extend(ws_map.clone());
                    }
                    map
                },
                fetch_pricing: ws.fetch_pricing.unwrap_or(base.fetch_pricing),
                zai_subscription: ws.zai_subscription.unwrap_or(base.zai_subscription),
                vision: ws.vision.clone().or_else(|| base.vision.clone()),
                transcription: ws
                    .transcription
                    .clone()
                    .or_else(|| base.transcription.clone()),
                image: ws.image.clone().or_else(|| base.image.clone()),
                video: ws.video.clone().or_else(|| base.video.clone()),
                speech: ws.speech.clone().or_else(|| base.speech.clone()),
                logins: ws.logins.clone().unwrap_or_else(|| base.logins.clone()),
            },
        }
    }

    /// Merge this workspace config into an effective CompactionConfig.
    /// Workspace config takes priority over the base (from agent section or global).
    pub fn merge_compaction(&self, base: &CompactionConfig) -> CompactionConfig {
        match &self.compaction {
            None => base.clone(),
            Some(ws) => CompactionConfig {
                max_context_tokens: ws.max_context_tokens.unwrap_or(base.max_context_tokens),
                compaction_threshold: ws.compaction_threshold.unwrap_or(base.compaction_threshold),
                keep_recent_messages: ws.keep_recent_messages.unwrap_or(base.keep_recent_messages),
                enable_memory_flush: ws.enable_memory_flush.unwrap_or(base.enable_memory_flush),
                prune_tool_outputs: ws.prune_tool_outputs.unwrap_or(base.prune_tool_outputs),
                prune_tool_output_max_chars: ws
                    .prune_tool_output_max_chars
                    .unwrap_or(base.prune_tool_output_max_chars),
                prune_tool_input_max_chars: ws
                    .prune_tool_input_max_chars
                    .unwrap_or(base.prune_tool_input_max_chars),
                max_compaction_passes: ws
                    .max_compaction_passes
                    .unwrap_or(base.max_compaction_passes),
                min_messages_to_compact: ws
                    .min_messages_to_compact
                    .unwrap_or(base.min_messages_to_compact),
                urgent_compaction_threshold: ws
                    .urgent_compaction_threshold
                    .unwrap_or(base.urgent_compaction_threshold),
                soft_eviction_threshold: ws
                    .soft_eviction_threshold
                    .unwrap_or(base.soft_eviction_threshold),
                compaction_model: ws
                    .compaction_model
                    .clone()
                    .or_else(|| base.compaction_model.clone()),
                flush_prompt: ws
                    .flush_prompt
                    .clone()
                    .or_else(|| base.flush_prompt.clone()),
                summary_prompt: ws
                    .summary_prompt
                    .clone()
                    .or_else(|| base.summary_prompt.clone()),
                rollover_warning_ratio: base.rollover_warning_ratio,
                rollover_keep_recent: base.rollover_keep_recent,
                stale_rollover_hours: ws.stale_rollover_hours.unwrap_or(base.stale_rollover_hours),
                voice_rollover_max_messages: ws
                    .voice_rollover_max_messages
                    .unwrap_or(base.voice_rollover_max_messages),
                chat_rollover_max_messages: ws
                    .chat_rollover_max_messages
                    .unwrap_or(base.chat_rollover_max_messages),
            },
        }
    }

    /// Merge workspace-level memory overrides onto the base memory config.
    pub fn merge_memory(&self, base: &MemoryConfig) -> MemoryConfig {
        match &self.memory {
            None => base.clone(),
            Some(ws) => MemoryConfig {
                backend: base.backend.clone(),
                db_path: base.db_path.clone(),
                embedding_provider: base.embedding_provider.clone(),
                embedding_model: base.embedding_model.clone(),
                embedding_api_url: base.embedding_api_url.clone(),
                embedding_api_key: base.embedding_api_key.clone(),
                passive: ws.passive.unwrap_or(base.passive),
                passive_model: ws
                    .passive_model
                    .clone()
                    .or_else(|| base.passive_model.clone()),
                passive_min_turn_tokens: ws
                    .passive_min_turn_tokens
                    .unwrap_or(base.passive_min_turn_tokens),
                passive_prompt: ws
                    .passive_prompt
                    .clone()
                    .or_else(|| base.passive_prompt.clone()),
                resume_context: ws.resume_context.unwrap_or(base.resume_context),
                relationship: ws.relationship.unwrap_or(base.relationship),
                relationship_min_turn_tokens: ws
                    .relationship_min_turn_tokens
                    .unwrap_or(base.relationship_min_turn_tokens),
                consolidation: ws.consolidation.unwrap_or(base.consolidation),
                consolidation_interval_secs: ws
                    .consolidation_interval_secs
                    .unwrap_or(base.consolidation_interval_secs),
                consolidation_lookback_secs: ws
                    .consolidation_lookback_secs
                    .unwrap_or(base.consolidation_lookback_secs),
                consolidation_model: ws
                    .consolidation_model
                    .clone()
                    .or_else(|| base.consolidation_model.clone()),
                consolidation_source: ws.consolidation_source.unwrap_or(base.consolidation_source),
                consolidation_cc_failover: ws
                    .consolidation_cc_failover
                    .unwrap_or(base.consolidation_cc_failover),
                lesson_extract: ws.lesson_extract.unwrap_or(base.lesson_extract),
                lesson_extract_model: ws
                    .lesson_extract_model
                    .clone()
                    .or_else(|| base.lesson_extract_model.clone()),
                lesson_extract_prompt: ws
                    .lesson_extract_prompt
                    .clone()
                    .or_else(|| base.lesson_extract_prompt.clone()),
                lesson_inject: ws.lesson_inject.unwrap_or(base.lesson_inject),
                lesson_inject_limit: ws.lesson_inject_limit.unwrap_or(base.lesson_inject_limit),
                lesson_inject_min_confidence: ws
                    .lesson_inject_min_confidence
                    .unwrap_or(base.lesson_inject_min_confidence),
                lesson_inject_min_importance: ws
                    .lesson_inject_min_importance
                    .unwrap_or(base.lesson_inject_min_importance),
                lesson_inject_min_score: ws
                    .lesson_inject_min_score
                    .unwrap_or(base.lesson_inject_min_score),
                lesson_inject_min_top_score: ws
                    .lesson_inject_min_top_score
                    .unwrap_or(base.lesson_inject_min_top_score),
                recall_min_certainty: ws.recall_min_certainty.or(base.recall_min_certainty),
                recall_recency_boost: ws.recall_recency_boost.unwrap_or(base.recall_recency_boost),
                recall_recency_half_life_hours: ws
                    .recall_recency_half_life_hours
                    .unwrap_or(base.recall_recency_half_life_hours),
                lesson_recall_alpha: ws.lesson_recall_alpha.or(base.lesson_recall_alpha),
                agent_recall_scope: ws.agent_recall_scope.unwrap_or(base.agent_recall_scope),
                lesson_decay: ws.lesson_decay.unwrap_or(base.lesson_decay),
                lesson_decay_idle_days: ws
                    .lesson_decay_idle_days
                    .unwrap_or(base.lesson_decay_idle_days),
                lesson_decay_step: ws.lesson_decay_step.unwrap_or(base.lesson_decay_step),
                lesson_decay_prune_floor: ws
                    .lesson_decay_prune_floor
                    .unwrap_or(base.lesson_decay_prune_floor),
                derived_figures: base.derived_figures.clone(),
            },
        }
    }

    /// Merge workspace-level advisor overrides onto the base advisor config.
    pub fn merge_advisor(&self, base: &AdvisorConfig) -> AdvisorConfig {
        match &self.advisor {
            None => base.clone(),
            Some(ws) => AdvisorConfig {
                enabled: ws.enabled.unwrap_or(base.enabled),
                source: ws.source.unwrap_or(base.source),
                payload: ws.payload.unwrap_or(base.payload),
                payload_max_tokens: ws.payload_max_tokens.unwrap_or(base.payload_max_tokens),
                per_turn_invocation_limit: ws
                    .per_turn_invocation_limit
                    .unwrap_or(base.per_turn_invocation_limit),
                model: ws.model.clone().or_else(|| base.model.clone()),
                cc_allow_failover_to_models: ws
                    .cc_allow_failover_to_models
                    .unwrap_or(base.cc_allow_failover_to_models),
                cc_fallback_model: ws
                    .cc_fallback_model
                    .clone()
                    .or_else(|| base.cc_fallback_model.clone()),
                max_rounds: ws.max_rounds.unwrap_or(base.max_rounds),
                cc_effort: ws.cc_effort.clone().or_else(|| base.cc_effort.clone()),
                agent_id: ws.agent_id.clone().or_else(|| base.agent_id.clone()),
                system_prompt_suffix: ws
                    .system_prompt_suffix
                    .clone()
                    .or_else(|| base.system_prompt_suffix.clone()),
                fanout: ws.fanout.clone().unwrap_or_else(|| base.fanout.clone()),
            },
        }
    }
}

/// Team definition in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    pub id: String,
    pub name: String,
    /// What this team does.
    #[serde(default)]
    pub description: Option<String>,
    /// Team archetype: "product", "engineering", "review", or "custom".
    #[serde(default)]
    pub team_type: Option<String>,
    pub lead: String,
    pub members: Vec<MemberConfig>,
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub require_review: bool,
    /// Path to shared team workspace for collaborative artifacts.
    #[serde(default)]
    pub shared_workspace: Option<String>,
}

/// Team member in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberConfig {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Expertise tags (e.g., "rust", "databases", "security", "ux").
    #[serde(default)]
    pub expertise: Vec<String>,
    /// Communication style: "concise", "thorough", "challenging", "supportive".
    #[serde(default)]
    pub communication_style: Option<String>,
    /// How eagerly this agent participates (0.0–1.0). Higher = more eager.
    #[serde(default)]
    pub priority_weight: Option<f64>,
    /// Personality dimensions that shape the agent's perspective and behaviour.
    #[serde(default)]
    pub personality: Option<PersonalityConfig>,
    /// Optional workspace directory for persistent identity and memory.
    /// When set, the member loads identity from SOUL.md, IDENTITY.md, AGENTS.md etc.
    /// in the workspace, gaining persistent personality and cross-session continuity.
    /// When absent, the member remains lightweight and stateless (existing behavior).
    #[serde(default)]
    pub workspace: Option<String>,
}

/// Personality dimensions for agent differentiation.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersonalityConfig {
    /// How strongly the agent pushes its viewpoint (0.0–1.0).
    #[serde(default)]
    pub assertiveness: Option<f64>,
    /// How often the agent asks probing questions (0.0–1.0).
    #[serde(default)]
    pub curiosity: Option<f64>,
    /// How much the agent favours practical over ideal solutions (0.0–1.0).
    #[serde(default)]
    pub pragmatism: Option<f64>,
    /// How deep the agent goes into specifics (0.0–1.0).
    #[serde(default)]
    pub detail_orientation: Option<f64>,
}

/// Built-in team templates for common configurations.
pub fn team_templates() -> Vec<(&'static str, &'static str, Vec<MemberConfig>)> {
    vec![
        (
            "product",
            "Product Team — brainstorming, specs, and user research",
            vec![
                MemberConfig {
                    id: "pm".into(),
                    role: "product_manager".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["planning".into(), "writing".into()],
                    expertise: vec!["product".into(), "roadmap".into(), "users".into()],
                    communication_style: Some("thorough".into()),
                    priority_weight: Some(0.9),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.7),
                        curiosity: Some(0.9),
                        pragmatism: Some(0.6),
                        detail_orientation: Some(0.5),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "ux".into(),
                    role: "ux_researcher".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["design".into(), "research".into()],
                    expertise: vec!["ux".into(), "design".into(), "accessibility".into()],
                    communication_style: Some("supportive".into()),
                    priority_weight: Some(0.7),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.4),
                        curiosity: Some(0.8),
                        pragmatism: Some(0.5),
                        detail_orientation: Some(0.7),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "architect".into(),
                    role: "architect".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["design".into(), "code-review".into()],
                    expertise: vec!["architecture".into(), "scalability".into(), "api".into()],
                    communication_style: Some("challenging".into()),
                    priority_weight: Some(0.8),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.8),
                        curiosity: Some(0.6),
                        pragmatism: Some(0.7),
                        detail_orientation: Some(0.8),
                    }),
                    workspace: None,
                },
            ],
        ),
        (
            "dev",
            "Dev Team — implementation, testing, and code review",
            vec![
                MemberConfig {
                    id: "tech-lead".into(),
                    role: "tech_lead".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["code-review".into(), "planning".into()],
                    expertise: vec!["architecture".into(), "code-review".into()],
                    communication_style: Some("concise".into()),
                    priority_weight: Some(0.9),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.7),
                        curiosity: Some(0.5),
                        pragmatism: Some(0.8),
                        detail_orientation: Some(0.6),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "senior-dev".into(),
                    role: "senior_dev".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["code-write".into(), "debug".into()],
                    expertise: vec!["implementation".into(), "performance".into()],
                    communication_style: Some("thorough".into()),
                    priority_weight: Some(0.8),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.5),
                        curiosity: Some(0.4),
                        pragmatism: Some(0.9),
                        detail_orientation: Some(0.7),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "reviewer".into(),
                    role: "reviewer".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["code-review".into(), "security-review".into()],
                    expertise: vec![
                        "code-review".into(),
                        "security".into(),
                        "best-practices".into(),
                    ],
                    communication_style: Some("challenging".into()),
                    priority_weight: Some(0.7),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.8),
                        curiosity: Some(0.6),
                        pragmatism: Some(0.5),
                        detail_orientation: Some(0.9),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "qa".into(),
                    role: "qa".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["test".into(), "debug".into()],
                    expertise: vec!["testing".into(), "edge-cases".into(), "reliability".into()],
                    communication_style: Some("thorough".into()),
                    priority_weight: Some(0.6),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.6),
                        curiosity: Some(0.7),
                        pragmatism: Some(0.6),
                        detail_orientation: Some(0.9),
                    }),
                    workspace: None,
                },
            ],
        ),
        (
            "review",
            "Review Team — architecture review, security audit, code quality",
            vec![
                MemberConfig {
                    id: "architect".into(),
                    role: "architect".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["design".into(), "code-review".into()],
                    expertise: vec![
                        "architecture".into(),
                        "scalability".into(),
                        "patterns".into(),
                    ],
                    communication_style: Some("challenging".into()),
                    priority_weight: Some(0.9),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.8),
                        curiosity: Some(0.6),
                        pragmatism: Some(0.7),
                        detail_orientation: Some(0.8),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "security".into(),
                    role: "security_reviewer".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["security-review".into()],
                    expertise: vec!["security".into(), "owasp".into(), "auth".into()],
                    communication_style: Some("challenging".into()),
                    priority_weight: Some(0.8),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.9),
                        curiosity: Some(0.7),
                        pragmatism: Some(0.4),
                        detail_orientation: Some(0.9),
                    }),
                    workspace: None,
                },
                MemberConfig {
                    id: "senior-dev".into(),
                    role: "senior_dev".into(),
                    model: None,
                    system_prompt: None,
                    capabilities: vec!["code-review".into(), "code-write".into()],
                    expertise: vec!["implementation".into(), "readability".into()],
                    communication_style: Some("supportive".into()),
                    priority_weight: Some(0.7),
                    personality: Some(PersonalityConfig {
                        assertiveness: Some(0.4),
                        curiosity: Some(0.5),
                        pragmatism: Some(0.8),
                        detail_orientation: Some(0.6),
                    }),
                    workspace: None,
                },
            ],
        ),
    ]
}

/// Get a rich role-perspective system prompt fragment for a given role.
///
/// Returns `None` if the role is not recognised; callers should fall back to a
/// generic description in that case.
pub fn role_system_prompt(role: &str) -> Option<&'static str> {
    match role {
        "product_manager" | "pm" => Some(
            "You are a product manager. You are user-obsessed: every decision must \
             trace back to real user value. Question assumptions. Push for simplicity \
             over feature creep. Think in user stories and outcomes. Challenge scope \
             creep with \"but why does the user need this?\". When the team proposes \
             solutions, ask who benefits and how we'll know it worked.",
        ),
        "architect" | "tech_lead" => Some(
            "You are a systems architect. Think in data flows, service boundaries, \
             and failure modes. Flag both over-engineering AND under-engineering. \
             Consider scalability, maintainability, and operational complexity. \
             Ask \"how does this fit the whole system?\" and \"what breaks at 10x scale?\". \
             Propose diagrams and architecture decision records for non-trivial choices.",
        ),
        "ux_designer" | "ux_researcher" | "ux" => Some(
            "You are a UX researcher and user advocate. Think about the user's mental \
             model, not the system's data model. Challenge technical jargon — if it \
             can't be explained simply, the design might be wrong. Consider edge cases, \
             accessibility, and error states. Ask \"what's the user's mental model here?\" \
             and \"what happens when things go wrong?\".",
        ),
        "senior_dev" | "developer" | "dev" => Some(
            "You are a pragmatic senior developer. Think about implementation \
             complexity, testability, and maintainability. Suggest simpler alternatives \
             when designs are over-engineered. Ask \"how will we test this?\" and \
             \"what's the simplest approach that works?\". Favour proven patterns over \
             novel solutions. Consider debugging and deployment complexity.",
        ),
        "security_reviewer" | "security" => Some(
            "You are a security reviewer. Think about attack surfaces, data flows, \
             and trust boundaries. Be constructively paranoid — assume adversarial \
             input. Ask \"what could go wrong?\" and \"who has access to this?\". \
             Check for OWASP top 10, auth/authz boundaries, data exposure, and \
             injection vectors. Flag risks clearly with severity.",
        ),
        "qa" | "tester" => Some(
            "You are a QA engineer. Think about test coverage, regression risk, and \
             edge cases. Ask \"what about when X happens?\" for unusual inputs, \
             timing issues, and boundary conditions. Push for clear acceptance \
             criteria. Challenge assumptions about happy paths. Consider test \
             automation, data setup, and environment consistency.",
        ),
        "reviewer" | "code_reviewer" => Some(
            "You are a code reviewer. Focus on code quality, readability, and \
             adherence to established patterns. Flag inconsistencies, missing error \
             handling, and naming issues. Consider performance implications and \
             API design. Ask \"is this consistent with how we do X elsewhere?\" \
             and \"what does this look like from the caller's perspective?\".",
        ),
        _ => None,
    }
}

/// Recognised team coordination patterns.
pub const TEAM_PATTERNS: &[&str] = &["hierarchical", "peer", "review", "swarm"];

/// Validate that `pattern` is a recognised team pattern.
pub fn validate_team_pattern(pattern: &str) -> eyre::Result<()> {
    if TEAM_PATTERNS.contains(&pattern) {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "Unknown team pattern '{}'. Valid patterns: {}",
            pattern,
            TEAM_PATTERNS.join(", ")
        ))
    }
}

/// Budget limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Master gate for spend enforcement and the estimate-confirm flow.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Hub-wide daily LLM spend ceiling in USD. 0 = unlimited.
    #[serde(default = "default_budget_hub_daily_usd")]
    pub hub_daily_usd: f64,
    /// Per-agent daily LLM spend ceiling in USD. 0 = unlimited.
    #[serde(default = "default_budget_agent_daily_usd")]
    pub agent_daily_usd: f64,
    /// Per-identity (human user) daily LLM spend ceiling in USD. 0 = unlimited.
    #[serde(default = "default_budget_identity_daily_usd")]
    pub identity_daily_usd: f64,
    /// Cumulative USD a single owned scheduler job may spend before it is
    /// auto-disabled. 0 = unlimited.
    #[serde(default = "default_budget_job_total_usd")]
    pub job_total_usd: f64,
    /// Total runs an owned recurring scheduler job may fire before it is
    /// auto-disabled. 0 = unlimited.
    #[serde(default = "default_budget_job_max_runs")]
    pub job_max_runs: u64,
    /// Days after which an owned recurring scheduler job expires and is
    /// auto-disabled. 0 = never.
    #[serde(default = "default_budget_job_expiry_days")]
    pub job_expiry_days: u32,
    /// Minimum interval in seconds for owned recurring schedules (interval
    /// and cron alike). System-registered jobs are exempt.
    #[serde(default = "default_budget_min_recurring_interval_secs")]
    pub min_recurring_interval_secs: u64,
    /// Projected daily USD above which creating recurring work requires
    /// explicit user confirmation with the estimate. 0 = always confirm.
    #[serde(default = "default_budget_confirm_recurring_daily_usd")]
    pub confirm_recurring_daily_usd: f64,
    /// Forecast USD above which a single task is flagged as expensive.
    #[serde(default = "default_budget_warn_turn_usd")]
    pub warn_turn_usd: f64,
    /// Fraction of a daily ceiling at which a once-per-day soft warning is
    /// emitted for the scope.
    #[serde(default = "default_budget_soft_warn_ratio")]
    pub soft_warn_ratio: f64,
    /// Identities whose spend is never capped per-identity (system
    /// maintenance). The hub ceiling still applies to everyone.
    #[serde(default = "default_budget_exempt_identities")]
    pub exempt_identities: Vec<String>,
    /// Per-agent tightening, keyed by agent id. Values can only lower the
    /// hub-wide ceilings (merged with `min`), never raise them. Lives here —
    /// not under `[[agents]]` — so runtime agent-entry rewrites can never
    /// drop it, and the whole `[budget]` table stays outside the overlay
    /// allowlist (config-file-only by construction).
    #[serde(default)]
    pub agents: HashMap<String, AgentBudgetOverride>,
}

/// Per-agent `[budget.agents.<id>]` tightening. Absent fields inherit the
/// hub-wide value; present fields are combined with `min` (0 = unlimited
/// counts as the largest value, so an override of 0 cannot loosen a cap).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentBudgetOverride {
    #[serde(default)]
    pub agent_daily_usd: Option<f64>,
    #[serde(default)]
    pub job_total_usd: Option<f64>,
    #[serde(default)]
    pub job_max_runs: Option<u64>,
    #[serde(default)]
    pub job_expiry_days: Option<u32>,
}

/// The budget ceilings in force for one agent after per-agent tightening.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveBudget {
    pub agent_daily_usd: f64,
    pub job_total_usd: f64,
    pub job_max_runs: u64,
    pub job_expiry_days: u32,
}

/// `min` where the zero default means unlimited on either side.
fn tighten<T: PartialOrd + Default>(base: T, over: Option<T>) -> T {
    match over {
        Some(o) if o > T::default() => {
            if base > T::default() && base < o {
                base
            } else {
                o
            }
        }
        _ => base,
    }
}

impl BudgetConfig {
    /// Resolve the ceilings for one agent: hub-wide values tightened by any
    /// `[budget.agents.<id>]` entry.
    pub fn for_agent(&self, agent_id: &str) -> EffectiveBudget {
        let over = self.agents.get(agent_id);
        EffectiveBudget {
            agent_daily_usd: tighten(self.agent_daily_usd, over.and_then(|o| o.agent_daily_usd)),
            job_total_usd: tighten(self.job_total_usd, over.and_then(|o| o.job_total_usd)),
            job_max_runs: tighten(self.job_max_runs, over.and_then(|o| o.job_max_runs)),
            job_expiry_days: tighten(self.job_expiry_days, over.and_then(|o| o.job_expiry_days)),
        }
    }
}

fn default_budget_hub_daily_usd() -> f64 {
    1000.0
}

fn default_budget_agent_daily_usd() -> f64 {
    200.0
}

fn default_budget_identity_daily_usd() -> f64 {
    50.0
}

fn default_budget_job_total_usd() -> f64 {
    25.0
}

fn default_budget_job_max_runs() -> u64 {
    100
}

fn default_budget_job_expiry_days() -> u32 {
    7
}

fn default_budget_min_recurring_interval_secs() -> u64 {
    300
}

fn default_budget_confirm_recurring_daily_usd() -> f64 {
    5.0
}

fn default_budget_warn_turn_usd() -> f64 {
    1.0
}

fn default_budget_soft_warn_ratio() -> f64 {
    0.8
}

fn default_budget_exempt_identities() -> Vec<String> {
    vec!["system:consolidation".to_string()]
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hub_daily_usd: default_budget_hub_daily_usd(),
            agent_daily_usd: default_budget_agent_daily_usd(),
            identity_daily_usd: default_budget_identity_daily_usd(),
            job_total_usd: default_budget_job_total_usd(),
            job_max_runs: default_budget_job_max_runs(),
            job_expiry_days: default_budget_job_expiry_days(),
            min_recurring_interval_secs: default_budget_min_recurring_interval_secs(),
            confirm_recurring_daily_usd: default_budget_confirm_recurring_daily_usd(),
            warn_turn_usd: default_budget_warn_turn_usd(),
            soft_warn_ratio: default_budget_soft_warn_ratio(),
            exempt_identities: default_budget_exempt_identities(),
            agents: HashMap::new(),
        }
    }
}

/// Gateway server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Gateway listen address (separate from admin API).
    #[serde(default = "default_gateway_listen")]
    pub listen: String,
    /// Authentication token for API/WS access.
    #[serde(default)]
    pub auth_token: Option<String>,
    /// Independent HS256 signing key for session JWTs. When unset, the JWT
    /// secret falls back to `auth_token` / `WORKFORCE_GATEWAY_TOKEN` (with a
    /// startup warning) — but reusing the gateway token means a leaked token
    /// can forge admin JWTs for any identity (audit F4). Set an independent
    /// ≥32-byte secret (e.g. via `WORKFORCE_JWT_SECRET`) to separate the keys.
    #[serde(default)]
    pub jwt_secret: Option<String>,
    /// Enable CORS (for web clients).
    #[serde(default = "default_true")]
    pub cors: bool,
    /// Explicit allow-list of origins for cross-origin requests. When
    /// `cors = true` and this list is empty no Access-Control headers are
    /// emitted (origins are locked down to same-origin), preventing the
    /// permissive-CORS CSRF surface that DW-517 closed. Operators expose web
    /// clients by listing their exact origin (e.g. `https://app.example.com`).
    /// Wildcard `*` is accepted but disables credentialed requests.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
    /// Optional OIDC / OAuth2 identity provider for SSO login (e.g. Keycloak).
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen: default_gateway_listen(),
            auth_token: None,
            jwt_secret: None,
            cors: true,
            cors_allowed_origins: Vec::new(),
            oidc: None,
        }
    }
}

/// Role to assign to users auto-provisioned via OIDC. Mirrors
/// `workforce_identity::types::IdentityRole` at the config layer so the
/// common crate does not need to depend on the identity crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OidcDefaultRole {
    SystemAdmin,
    Manager,
    #[default]
    Member,
}

/// Configuration for an optional OIDC/OAuth2 identity provider.
///
/// When `enabled = true` the login page renders a "Login with {display_name}"
/// button and the hub accepts an Authorization Code + PKCE flow against the
/// configured issuer. The workforce hub continues to issue its own internal
/// JWTs; the IdP is only consulted at login time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    /// Turn the feature on. When false the provider is not discovered and
    /// no OIDC endpoints are exposed.
    #[serde(default)]
    pub enabled: bool,
    /// Human-readable name shown on the login button (e.g. "Keycloak").
    pub display_name: String,
    /// OIDC issuer URL — the hub discovers the rest of the endpoints via
    /// `{issuer_url}/.well-known/openid-configuration`.
    pub issuer_url: String,
    /// OAuth2 client id registered with the IdP.
    pub client_id: String,
    /// OAuth2 client secret — supports `${vault:...}` expansion.
    pub client_secret: String,
    /// Exact redirect URI registered with the IdP. Must point at the
    /// hub's `/api/auth/oidc/callback` endpoint on the public gateway URL.
    pub redirect_uri: String,
    /// Optional email-suffix allowlist. If non-empty, only users whose
    /// verified email ends with one of these suffixes (case-insensitive)
    /// can log in. Empty = allow any verified email.
    #[serde(default)]
    pub allowed_email_domains: Vec<String>,
    /// Role assigned to auto-provisioned users on first OIDC login.
    #[serde(default)]
    pub default_role: OidcDefaultRole,
    /// When true, create a workforce identity on first OIDC login if one
    /// does not already exist for that email. When false, the user must be
    /// pre-provisioned (login fails otherwise).
    #[serde(default = "default_true")]
    pub auto_create_users: bool,
}

/// MCP (Model Context Protocol) configuration.
///
/// Two distinct surfaces share this section:
///
/// 1. **Workforce-as-MCP-server**: when `enabled = true`, workflow
///    engines spawn a per-step stdio MCP helper that exposes a
///    curated subset of the calling agent's tools to Claude Code.
///    `exposed_tools` controls the curation. The `listen` and
///    `auth_token` fields are retained for config compatibility
///    and are unused by the stdio transport.
///
/// 2. **Workforce-as-MCP-client**: each entry under `[mcp.servers.*]`
///    declares an upstream remote-HTTPS MCP server. At hub startup
///    each server is initialised, its `tools/list` fetched, and
///    every tool registered as `mcp.<server_id>.<tool_name>` in the
///    agent's tool registry. See [`McpServerConfig`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Schema version. Bumped only on breaking config-shape changes;
    /// gives operators a fence for future TOML→DB migrations.
    #[serde(default = "default_mcp_schema_version")]
    pub schema_version: u32,
    /// Whether the workforce-as-MCP-server stdio helper is enabled
    /// for workflow engines. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Tool name patterns to expose to workflow engines.
    /// Supports exact names (`"knowledge.ask"`) and prefix globs (`"knowledge.*"`).
    /// Empty = expose all tools in the calling agent's registry.
    #[serde(default)]
    pub exposed_tools: Vec<String>,
    /// Upstream MCP servers the hub should connect to as a client.
    /// Keyed by `server_id` (the user-chosen handle that becomes the
    /// `mcp.<server_id>.*` tool prefix and the vault path segment).
    #[serde(default)]
    pub servers: std::collections::HashMap<String, McpServerConfig>,
}

fn default_mcp_schema_version() -> u32 {
    1
}

impl McpConfig {
    /// The tool patterns workflow engines are served: none when MCP is off,
    /// and everything — `*` — when it is on and names none. Read here, once,
    /// so a served surface never has to guess whether an empty list means
    /// all or nothing.
    pub fn step_patterns(&self) -> Option<Vec<String>> {
        self.enabled.then(|| match self.exposed_tools.is_empty() {
            true => vec!["*".to_string()],
            false => self.exposed_tools.clone(),
        })
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            schema_version: default_mcp_schema_version(),
            enabled: false,
            exposed_tools: Vec::new(),
            servers: std::collections::HashMap::new(),
        }
    }
}

/// `[remote_mcp]` — the hub as an MCP server for external clients (Claude Code,
/// `workforce-local-mcp`) authenticating with per-user `wfr_` tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteMcpConfig {
    /// Off by default: the `/mcp` routes answer 404 until enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Lifetime of a minted token in days.
    #[serde(default = "default_remote_mcp_token_ttl_days")]
    pub token_ttl_days: u32,
    /// Live (unexpired, unrevoked) tokens one identity may hold.
    #[serde(default = "default_remote_mcp_max_tokens_per_user")]
    pub max_tokens_per_user: u32,
    /// Tool name patterns exposed over remote MCP (exact names or `prefix.*`);
    /// the capability gate still applies underneath. `["*"]` exposes the whole
    /// registry.
    #[serde(default = "default_remote_mcp_tools_allow")]
    pub tools_allow: Vec<String>,
    /// How long a call waits for an agent turn before returning a poll handle,
    /// and the longest a call answered as one JSON body may wait. Keep it
    /// below any reverse proxy's idle timeout (ALB defaults to 60s), or the
    /// caller loses the poll handle when the proxy drops the request.
    #[serde(default = "default_remote_mcp_chat_default_wait_secs")]
    pub chat_default_wait_secs: u64,
    /// Upper bound a caller may request for a wait answered on an SSE stream,
    /// whose keepalives outlast a proxy's idle timeout.
    #[serde(default = "default_remote_mcp_chat_max_wait_secs")]
    pub chat_max_wait_secs: u64,
}

impl Default for RemoteMcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            token_ttl_days: default_remote_mcp_token_ttl_days(),
            max_tokens_per_user: default_remote_mcp_max_tokens_per_user(),
            tools_allow: default_remote_mcp_tools_allow(),
            chat_default_wait_secs: default_remote_mcp_chat_default_wait_secs(),
            chat_max_wait_secs: default_remote_mcp_chat_max_wait_secs(),
        }
    }
}

impl RemoteMcpConfig {
    /// Tool patterns exposed by default: authoring and running workflows,
    /// playbooks, schedules and skills of both kinds, vault access, the `agent.*`
    /// collaboration tools, the `approval.*` bridge and `help`. Shell, code
    /// and delivery tools stay opt-in.
    pub const DEFAULT_TOOLS_ALLOW: &'static [&'static str] = &[
        "workflow",
        "playbook",
        "scheduler",
        "reminder",
        "wasm_skill.*",
        "vault.*",
        "secret.list",
        "identity.whoami",
        "system.list_skills",
        "system.enable_skill",
        "system.install_external_skill",
        "system.uninstall_external_skill",
        "system.list_external_skills",
        "system.publish_external_skill",
        "skills.*",
        "agent.*",
        "approval.*",
        "skill.*",
        "pipeline.*",
        "hubspot.*",
        "help",
    ];

    /// Effective wait on an agent turn: the caller's request, or the default
    /// when unspecified, clamped to the ceiling for how the call answers.
    pub fn chat_wait_secs(&self, requested: Option<u64>, streamed: bool) -> u64 {
        let ceiling = if streamed {
            self.chat_max_wait_secs
        } else {
            self.chat_default_wait_secs
        };
        requested
            .unwrap_or(self.chat_default_wait_secs)
            .min(ceiling)
    }
}

fn default_remote_mcp_token_ttl_days() -> u32 {
    90
}

fn default_remote_mcp_max_tokens_per_user() -> u32 {
    10
}

fn default_remote_mcp_tools_allow() -> Vec<String> {
    RemoteMcpConfig::DEFAULT_TOOLS_ALLOW
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_remote_mcp_chat_default_wait_secs() -> u64 {
    50
}

fn default_remote_mcp_chat_max_wait_secs() -> u64 {
    600
}

/// One upstream MCP server. The `server_id` is the map key in
/// `[mcp.servers.<server_id>]` and is *not* duplicated here — it
/// becomes the tool prefix (`mcp.<server_id>.*`) and the vault path
/// segment (`mcp/v1/<server_id>/...` for OAuth credentials).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Full HTTPS endpoint, including any trailing path the vendor
    /// expects (e.g. `https://mcp.linear.app/mcp`).
    pub endpoint: String,
    /// Transport flavour. Only `"streamable-http"` is supported in
    /// v1; explicit so adding `stdio` later is a non-breaking
    /// config change.
    #[serde(default = "default_mcp_server_transport")]
    pub transport: String,
    /// Credential resolution. See [`McpAuthConfig`].
    pub auth: McpAuthConfig,
    /// Hostnames the client is allowed to reach. Defence-in-depth
    /// for misconfigurations / vendor URL changes; empty means
    /// any host (not recommended).
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// How often to refetch `tools/list`. Default 1 hour.
    #[serde(default = "default_mcp_refresh_interval_secs")]
    pub refresh_interval_secs: u64,
    /// Tool-name glob patterns whose matching built-in tools should
    /// be hidden from the LLM when this server is the active surface
    /// for the calling user — e.g. `["linear_*"]` to cede the Linear
    /// surface to the upstream MCP server. Decision is per-request,
    /// driven by [`super::settings::McpServerConfig::fallback_to_builtin`]
    /// and the user's OAuth state.
    #[serde(default)]
    pub shadows: Vec<String>,
    /// When the calling user has no token for this MCP server, fall
    /// back to the shadowed built-in skill if it's loaded. Default
    /// `false` — the user instead sees the MCP surface with a
    /// graceful "please authorize" error on call, preserving the
    /// per-user audit trail OAuth provides.
    #[serde(default)]
    pub fallback_to_builtin: bool,
    /// Glob patterns (`*`, `?`, `[...]`) matched against each upstream
    /// tool's *raw* name — the name the server advertises (e.g.
    /// `issue_create`), not the `mcp.<server_id>.` qualified form. When
    /// non-empty, ONLY matching tools are registered: an allowlist that
    /// curates how much of a large server's surface enters the agent's
    /// tool registry, and therefore how many schemas a large-context
    /// model carries inline per request. The filter is applied during the
    /// `tools/list` fetch, so the registration cap counts only the kept
    /// tools — a server with more tools than the cap still connects once
    /// the allowlist trims it below. Empty (default) registers every tool,
    /// backward-compatible with records persisted before this field
    /// existed.
    #[serde(default)]
    pub tools_allow: Vec<String>,
    /// Glob patterns matched against each upstream tool's raw name; any
    /// match is dropped even if it also matched `tools_allow` (deny wins).
    /// Use to subtract a handful of tools from an otherwise-allowed
    /// surface (e.g. allow `issue_*`, deny `issue_delete`).
    #[serde(default)]
    pub tools_deny: Vec<String>,
    /// Workspace (group) this server belongs to, e.g. `"harbor"`. When
    /// set, the server is **gated**: its tools are callable only when its
    /// [`Self::env`] is the workspace's *active* env — a session
    /// `cmd: use <ws> <env>` override if present, else the `default` member,
    /// else none (every member dormant — the safe empty state). `None`
    /// (default) = ungrouped, always active, identical to every install made
    /// before this field existed. Grouping is the opt-in that makes a server
    /// (e.g. a lone staging install) stop being auto-reached for prod work.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Environment label within [`Self::workspace`], e.g. `"staging"` /
    /// `"prod"` / `"demo"`. Required (non-empty) when `workspace` is set;
    /// ignored otherwise. Members of one workspace are mutually exclusive by
    /// env — exactly one env is active at a time, so tool names stay
    /// env-agnostic (`mcp_<server_id>_*`) and never collide across envs.
    #[serde(default)]
    pub env: Option<String>,
    /// Whether this member is the workspace's **default** (auto-active) env.
    /// At most one member per workspace should be `true`. Flipping it is a
    /// human-gated action (`cmd: default <ws> <env>`), never agent-driven —
    /// the default is auto-active, so an agent able to set it could point a
    /// prod investigation at staging, the exact failure grouping prevents.
    #[serde(default)]
    pub default: bool,
}

fn default_mcp_server_transport() -> String {
    "streamable-http".to_string()
}

fn default_mcp_refresh_interval_secs() -> u64 {
    3600
}

/// How an MCP server's bearer token is resolved. `kind` discriminator
/// drives the variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpAuthConfig {
    /// Static personal-access-token / API key. Read on every call
    /// from `vault_path` via `LayeredUserVault` so a per-user
    /// override (`users/{uid}/<vault_path>`) wins when present —
    /// PAT auth's User/Shared distinction is implicit in the
    /// vault layering, no explicit binding needed.
    Pat { vault_path: String },
    /// OAuth 2.1 — operator seeds `mcp/v1/<server_id>/client_id` +
    /// `client_secret` + `redirect_uri` in the global vault; per-user
    /// tokens land at `users/{uid}/mcp/v1/<server_id>/*` after the
    /// `/api/auth/<provider>/callback` flow.
    Oauth2 {
        /// Per-user (default) vs shared vault scope for the persisted
        /// access/refresh tokens. Per-user requires an active
        /// `CURRENT_USER_ID` task-local at tool-call time.
        #[serde(default = "default_mcp_auth_binding")]
        binding: McpAuthBinding,
        /// Stable provider identifier — must match a registered
        /// [`workforce_oauth_client::OAuthProvider`] impl (e.g.
        /// `"linear"`).
        provider: String,
        /// Override scopes. Empty = use provider's defaults.
        #[serde(default)]
        scopes: Vec<String>,
        /// Optional shared-PAT fallback. When set, each call resolves
        /// the calling user's own OAuth token first; if they have none,
        /// the static token at this vault path is used; if neither
        /// resolves, the call fails as unconfigured. Lets one server
        /// serve per-user clients and a shared org-wide token at once.
        #[serde(default)]
        shared_pat_fallback: Option<String>,
        /// Cached authorization-server metadata for self-hosted servers
        /// that advertise their OAuth endpoints via RFC 8414 discovery
        /// (e.g. Metabase). `None` for static providers, whose endpoints
        /// come from the registered `provider` impl. Populated once at
        /// install time and read on every authorize/refresh, so
        /// `provider` here is just a vault-path/log identifier.
        #[serde(default)]
        discovery: Option<DiscoveredMetadata>,
    },
}

/// Authorization-server metadata cached on an OAuth MCP server config
/// after RFC 8414 discovery. Endpoints are per-instance for self-hosted
/// services, so they're stored rather than hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthBinding {
    /// Per-user vault scope. Token reads require an active
    /// `CURRENT_USER_ID` task-local; missing user fails the call
    /// (no silent fallback to a shared bucket).
    User,
    /// Single token shared across every user of this server (e.g.
    /// org-wide Datadog API key).
    Shared,
}

fn default_mcp_auth_binding() -> McpAuthBinding {
    McpAuthBinding::User
}

/// Default tool patterns exposed to `/cc` Claude Code chat when
/// `[claude_chat].exposed_tools` is empty. Curated to exclude hub-internal
/// admin surfaces (`vault.*`, `system.*`, `workflow.*`, `slack.*`,
/// `engagement.*`) that could rewrite agent state from a user thread.
///
/// `memory` / `notes` are exact names, not prefixes — the tools register
/// under bare action-style names. mcp-serve serves `memory` only when the
/// install runs the real (Weaviate) store; elsewhere the namespace is
/// honestly reported as unserved rather than recalling from an empty
/// ephemeral store.
pub const DEFAULT_CHAT_EXPOSED_TOOLS: &[&str] = &[
    "knowledge.*",
    "context.*",
    "linear.*",
    "jira.*",
    "metabase.*",
    "google.*",
    "github.*",
    "datadog.*",
    "code.*",
    "memory",
    "notes",
    "postmark.*",
    "sendgrid.*",
    "hubspot.*",
    "notion.*",
    "browser.*",
    "email.*",
    // Authoring a skill is a member-level, everyday operation, so it belongs in
    // the default rather than behind a per-install override. Exposure is not
    // authority: the capability gate and per-skill ownership still decide what
    // the signed-in user may actually do with these.
    "wasm_skill.*",
];

/// `/cc` Claude Code chat configuration. Reuses the `[mcp]` stdio bridge
/// but with a tighter default allow-list since chat is user-driven.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeChatConfig {
    /// `None` inherits `[mcp].enabled`; explicit `Some(false)` disables
    /// chat MCP without affecting workflow MCP.
    #[serde(default)]
    pub mcp_enabled: Option<bool>,
    /// Tool name patterns (exact or `prefix.*`). Empty falls back to
    /// [`DEFAULT_CHAT_EXPOSED_TOOLS`].
    #[serde(default)]
    pub exposed_tools: Vec<String>,
    /// Claude Code model for `/cc` turns (`opus`, `sonnet`, `haiku`).
    /// `None` uses the subscription default. Validated by the CC CLI.
    #[serde(default)]
    pub model: Option<String>,
    /// Thinking effort for `/cc` turns (`low`, `medium`, `high`, `max`).
    /// `None` uses the CC default. Validated by the CC CLI.
    #[serde(default)]
    pub effort: Option<String>,
    /// Force-rotate the Claude Code session after this many completed
    /// turns. `None` uses the hub default (50). Rotation loses the thread's
    /// prior CC context, so set this high if continuity matters more than
    /// the per-turn prefix-replay cost.
    #[serde(default)]
    pub rotate_after_turns: Option<u64>,
    /// Force-rotate the Claude Code session after this many idle minutes.
    /// `None` uses the hub default (240).
    #[serde(default)]
    pub rotate_after_idle_mins: Option<i64>,
    /// Where `/cc` turns run: `inherit` follows `[coding_session.executor]`,
    /// `local` pins them to this host.
    #[serde(default)]
    pub executor: ChatExecutor,
    /// What runs `/cc` turns; absent `[claude_chat.engine]` follows
    /// `[coding_session.engine]`.
    #[serde(default)]
    pub engine: Option<CodingEngineConfig>,
}

/// Where `/cc` turns run relative to `[coding_session.executor]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatExecutor {
    #[default]
    Inherit,
    Local,
}

impl ClaudeChatConfig {
    pub fn effective_mcp_enabled(&self, mcp: &McpConfig) -> bool {
        self.mcp_enabled.unwrap_or(mcp.enabled)
    }

    /// The executor `/cc` uses, given the coding session's.
    pub fn effective_executor(&self, coding: &ExecutorConfig) -> ExecutorConfig {
        match self.executor {
            ChatExecutor::Inherit => coding.clone(),
            ChatExecutor::Local => ExecutorConfig {
                kind: ExecutorKind::Local,
                ..coding.clone()
            },
        }
    }

    /// The engine chain `/cc` uses, given the coding session's.
    pub fn effective_engine(&self, coding: &CodingEngineConfig) -> CodingEngineConfig {
        self.engine.clone().unwrap_or_else(|| coding.clone())
    }

    /// The engine chain a chat surface uses when its own config pins none:
    /// the coding session's, led by `primary`.
    pub fn effective_engine_led_by(
        &self,
        coding: &CodingEngineConfig,
        primary: CodingEngineKind,
    ) -> CodingEngineConfig {
        self.engine.clone().unwrap_or_else(|| CodingEngineConfig {
            kind: primary,
            ..coding.clone()
        })
    }

    pub fn effective_exposed_tools(&self) -> Vec<String> {
        patterns_or_default(&self.exposed_tools, DEFAULT_CHAT_EXPOSED_TOOLS)
    }
}

/// Marker that expands to the built-in defaults in place, so a config can add
/// to them instead of restating them.
pub const DEFAULT_PATTERNS_MARKER: &str = "@default";

/// Resolve a configured pattern list against its built-in default.
///
/// An empty list takes the default, and a plain list replaces it — both as
/// before. Two markers make the common edits expressible without copying the
/// default out into every config, where it then silently rots as the default
/// moves on:
///
/// * `"@default"` expands to the whole default list at that position, so
///   `["@default", "wasm_skill.*"]` means "everything usual, plus this".
/// * `"!pattern"` removes a pattern already accumulated, so
///   `["@default", "!browser.*"]` means "everything usual, except that".
///
/// Order matters: a removal only affects what precedes it, which is what lets
/// `["!jira.*", "@default"]` read as "drop it, then take the default back"
/// rather than as a contradiction. Duplicates collapse, keeping first position.
fn patterns_or_default(custom: &[String], default: &[&str]) -> Vec<String> {
    if custom.is_empty() {
        return default.iter().map(|s| (*s).to_string()).collect();
    }
    custom
        .iter()
        .fold(Vec::new(), |mut acc, entry| {
            match entry.strip_prefix('!') {
                Some(removed) => acc.retain(|p: &String| p != removed),
                None if entry == DEFAULT_PATTERNS_MARKER => {
                    acc.extend(default.iter().map(|s| (*s).to_string()))
                }
                None => acc.push(entry.clone()),
            }
            acc
        })
        .into_iter()
        .fold(Vec::new(), |mut seen, p| {
            if !seen.contains(&p) {
                seen.push(p);
            }
            seen
        })
}

#[cfg(test)]
mod pattern_resolution_tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }
    const DEF: &[&str] = &["a.*", "b.*"];

    #[test]
    fn empty_takes_the_default() {
        assert_eq!(patterns_or_default(&[], DEF), v(&["a.*", "b.*"]));
    }

    #[test]
    fn a_plain_list_still_replaces() {
        assert_eq!(patterns_or_default(&v(&["z.*"]), DEF), v(&["z.*"]));
    }

    #[test]
    fn marker_extends_the_default() {
        assert_eq!(
            patterns_or_default(&v(&["@default", "z.*"]), DEF),
            v(&["a.*", "b.*", "z.*"])
        );
    }

    #[test]
    fn marker_position_is_honoured() {
        assert_eq!(
            patterns_or_default(&v(&["z.*", "@default"]), DEF),
            v(&["z.*", "a.*", "b.*"])
        );
    }

    #[test]
    fn negation_removes_from_the_default() {
        assert_eq!(
            patterns_or_default(&v(&["@default", "!a.*"]), DEF),
            v(&["b.*"])
        );
    }

    #[test]
    fn negation_only_affects_what_precedes_it() {
        // "drop it, then take the default back" — not a contradiction.
        assert_eq!(
            patterns_or_default(&v(&["!a.*", "@default"]), DEF),
            v(&["a.*", "b.*"])
        );
    }

    #[test]
    fn negating_something_absent_is_a_no_op() {
        assert_eq!(
            patterns_or_default(&v(&["@default", "!nope.*"]), DEF),
            v(&["a.*", "b.*"])
        );
    }

    #[test]
    fn duplicates_collapse_keeping_first_position() {
        assert_eq!(
            patterns_or_default(&v(&["b.*", "@default"]), DEF),
            v(&["b.*", "a.*"])
        );
    }

    #[test]
    fn skill_authoring_is_exposed_to_chat_and_coding_sessions_by_default() {
        assert!(DEFAULT_CHAT_EXPOSED_TOOLS.contains(&"wasm_skill.*"));
        assert!(DEFAULT_CODING_SESSION_EXPOSED_TOOLS.contains(&"wasm_skill.*"));
    }
}

#[cfg(test)]
mod mcp_config_compat_tests {
    use super::*;

    /// `[mcp] listen`/`auth_token` were dead under the stdio transport and were
    /// removed when `mcp-serve` gained a real, per-session listener. Deployed
    /// TOML still sets them and the image ships ahead of the config, so they
    /// must parse harmlessly rather than fail the hub's boot.
    #[test]
    fn remote_mcp_section_defaults_and_overrides() {
        let defaults: WorkforceConfig = toml::from_str("").expect("empty config parses");
        assert!(!defaults.remote_mcp.enabled);
        assert_eq!(defaults.remote_mcp.token_ttl_days, 90);
        assert_eq!(defaults.remote_mcp.max_tokens_per_user, 10);
        assert_eq!(
            defaults.remote_mcp.tools_allow,
            RemoteMcpConfig::DEFAULT_TOOLS_ALLOW
        );
        assert_eq!(defaults.remote_mcp.chat_wait_secs(None, false), 50);
        assert_eq!(defaults.remote_mcp.chat_wait_secs(Some(30), false), 30);
        assert_eq!(defaults.remote_mcp.chat_wait_secs(Some(5_000), false), 50);
        assert_eq!(defaults.remote_mcp.chat_wait_secs(Some(5_000), true), 600);

        let cfg: WorkforceConfig = toml::from_str(
            r#"
            [remote_mcp]
            enabled = true
            token_ttl_days = 30
            tools_allow = ["*"]
            "#,
        )
        .expect("remote_mcp section parses");
        assert!(cfg.remote_mcp.enabled);
        assert_eq!(cfg.remote_mcp.token_ttl_days, 30);
        assert_eq!(cfg.remote_mcp.tools_allow, ["*"]);
        assert_eq!(cfg.remote_mcp.max_tokens_per_user, 10);
    }

    #[test]
    fn a_config_still_setting_the_removed_mcp_keys_parses() {
        let toml = r#"
            [mcp]
            enabled = true
            listen = "0.0.0.0:8092"
            auth_token = "stale"
            exposed_tools = ["knowledge.*"]
        "#;
        let cfg: WorkforceConfig = toml::from_str(toml).expect("stale keys must not fail the boot");
        assert!(cfg.mcp.enabled);
        assert_eq!(cfg.mcp.exposed_tools, vec!["knowledge.*".to_string()]);
    }
}

/// Default tool patterns exposed to `coding.session`'s Claude Code driver
/// when `[coding_session].exposed_tools` is empty. Read-leaning: issue
/// lookups, codebase context, and agent memory alongside the code work.
/// `memory` / `notes` are exact bare names (see
/// [`DEFAULT_CHAT_EXPOSED_TOOLS`]).
pub const DEFAULT_CODING_SESSION_EXPOSED_TOOLS: &[&str] = &[
    "linear.*",
    "jira.*",
    "context.*",
    "knowledge.*",
    "memory",
    "notes",
    // See the note on DEFAULT_CHAT_EXPOSED_TOOLS — a coding session that can
    // edit a skill should be able to build and install it too.
    "wasm_skill.*",
];

/// `coding.session` (agent-driven Claude Code) configuration. Also governs
/// `coding.fanout`, which shares the same workspace + tool surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodingSessionConfig {
    /// Tool name patterns (exact or `prefix.*`). Empty falls back to
    /// [`DEFAULT_CODING_SESSION_EXPOSED_TOOLS`].
    #[serde(default)]
    pub exposed_tools: Vec<String>,
    /// Max concurrent `coding.fanout` coders (Claude Code subprocesses) per
    /// fan-out call. Extra subtasks queue and run in waves. `0` falls back to
    /// the default. See [`Self::effective_fanout_max_parallel`].
    #[serde(default)]
    pub fanout_max_parallel: u32,
    /// Execution backend for a coding round. Absent `[coding_session.executor]`
    /// → `local`, i.e. unchanged behaviour.
    #[serde(default)]
    pub executor: ExecutorConfig,
    /// Turn cap for one coding round (`--max-turns`). A backstop against a
    /// runaway loop, not the operative bound — `timeout_secs` is. `0` falls back
    /// to [`DEFAULT_DRIVER_MAX_TURNS`].
    #[serde(default)]
    pub max_turns: u32,
    /// Wall clock for one `coding.session` / `coding.fanout` round, covering all
    /// of its attempts. `0` falls back to [`DEFAULT_DRIVER_TIMEOUT_SECS`].
    #[serde(default)]
    pub timeout_secs: u64,
    /// Install-specific operating notes appended to every round's system
    /// prompt — what this install's repos cannot know about the host: whether
    /// CI is the verifier, which checks are cheap here, what not to install.
    #[serde(default)]
    pub preamble: String,
    /// Persistent directory, writable by the user coding rounds run as, that
    /// holds toolchain caches (pnpm/npm/corepack, Cargo, Gradle, Bundler) across
    /// rounds and restarts. Point it at fast local disk; unset leaves every
    /// toolchain on its own default under the round's `HOME`.
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// Default `--model` for a coding round (alias or full id); a call's
    /// explicit `model` wins. Unset defers to the Claude Code CLI's default.
    #[serde(default)]
    pub model: Option<String>,
    /// Default `--effort` for a coding round (one of
    /// [`CLAUDE_CODE_EFFORTS`]); a call's explicit `effort` wins.
    #[serde(default)]
    pub effort: Option<String>,
    /// What runs a coding round and what it falls back to. Absent
    /// `[coding_session.engine]` → Claude Code on the subscription, no fallback.
    #[serde(default)]
    pub engine: CodingEngineConfig,
    /// How Claude Code reaches Bedrock when an engine entry names
    /// `claude_code_bedrock`.
    #[serde(default)]
    pub bedrock: CodingBedrockConfig,
    /// How the Codex CLI runs when an engine entry names `codex` or
    /// `codex_api`.
    #[serde(default)]
    pub codex: CodingCodexConfig,
}

/// `[coding_session.bedrock]` — the credentials and region Claude Code on
/// Bedrock runs with. A round gets its own short-lived credentials from
/// `assume_role_arn`, so nothing longer-lived than the round lands in a scope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingBedrockConfig {
    /// A role limited to invoking Bedrock models, assumed by the hub for each
    /// round with a lifetime matching the round's wall clock. Unset means the
    /// engine is not available on this hub.
    #[serde(default)]
    pub assume_role_arn: Option<String>,
    /// The Bedrock region; unset uses the hub's ambient AWS region.
    #[serde(default)]
    pub region: Option<String>,
    /// The cross-region inference-profile prefix put before a bare model id
    /// (`eu` turns `anthropic.claude-sonnet-5` into
    /// `eu.anthropic.claude-sonnet-5`); unset derives it from the region.
    #[serde(default)]
    pub region_prefix: Option<String>,
    /// The longest credential the role permits (its maximum session
    /// duration), in seconds. `0` falls back to
    /// [`DEFAULT_BEDROCK_MAX_SESSION_SECS`]; STS accepts 900 to 43200.
    #[serde(default)]
    pub max_session_secs: u64,
}

/// STS's default maximum session duration for an assumed role.
pub const DEFAULT_BEDROCK_MAX_SESSION_SECS: u64 = 3600;
const STS_SESSION_SECS: std::ops::RangeInclusive<u64> = 900..=43200;

impl CodingBedrockConfig {
    pub fn is_configured(&self) -> bool {
        self.assume_role_arn.is_some()
    }

    pub fn effective_max_session(&self) -> std::time::Duration {
        std::time::Duration::from_secs(match self.max_session_secs {
            0 => DEFAULT_BEDROCK_MAX_SESSION_SECS,
            secs => secs,
        })
    }

    fn validate(&self, chain_names_bedrock: bool) -> eyre::Result<()> {
        if let Some(arn) = &self.assume_role_arn {
            if !arn.starts_with("arn:aws:iam::") || !arn.contains(":role/") {
                return Err(eyre::eyre!(
                    "[coding_session.bedrock] assume_role_arn {arn:?} is not an IAM role ARN"
                ));
            }
        }
        if self.max_session_secs != 0 && !STS_SESSION_SECS.contains(&self.max_session_secs) {
            return Err(eyre::eyre!(
                "[coding_session.bedrock] max_session_secs must be between {} and {}, got {}",
                STS_SESSION_SECS.start(),
                STS_SESSION_SECS.end(),
                self.max_session_secs
            ));
        }
        if chain_names_bedrock && !self.is_configured() {
            return Err(eyre::eyre!(
                "[coding_session.engine] names claude_code_bedrock but [coding_session.bedrock] assume_role_arn is unset"
            ));
        }
        Ok(())
    }
}

/// `[coding_session.codex]` — how the Codex CLI runs. Auth is not here: the
/// subscription login lives in the vault as `codex/auth_json`, the metered
/// key as `openai/api_key`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingCodexConfig {
    /// How long one hub tool call may take before Codex gives up on it. `0`
    /// falls back to [`DEFAULT_CODEX_TOOL_TIMEOUT_SECS`].
    #[serde(default)]
    pub tool_timeout_secs: u64,
    /// Short model names a round may ask for, over the built-in
    /// [`DEFAULT_CODEX_MODELS`]; a name not listed is passed to the CLI as a
    /// model id.
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, String>,
}

/// Codex's own default for an MCP tool call is a minute, which a Datadog or
/// Metabase query can exceed.
pub const DEFAULT_CODEX_TOOL_TIMEOUT_SECS: u64 = 300;

/// The short names a round may use for Codex models, and what they resolve to.
pub const DEFAULT_CODEX_MODELS: &[(&str, &str)] = &[
    ("astra", "gpt-6-astra"),
    ("sol", "gpt-5.6-sol"),
    ("terra", "gpt-5.6-terra"),
    ("luna", "gpt-5.6-luna"),
];

impl CodingCodexConfig {
    pub fn effective_tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(match self.tool_timeout_secs {
            0 => DEFAULT_CODEX_TOOL_TIMEOUT_SECS,
            secs => secs,
        })
    }

    /// Every short name and its model id: the defaults, overridden and
    /// extended by `models`.
    pub fn model_aliases(&self) -> std::collections::BTreeMap<String, String> {
        DEFAULT_CODEX_MODELS
            .iter()
            .map(|(alias, id)| (alias.to_string(), id.to_string()))
            .chain(self.models.iter().map(|(a, i)| (a.clone(), i.clone())))
            .collect()
    }

    /// The model id for `spec`: its alias's id, else `spec` itself.
    pub fn resolve_model(&self, spec: &str) -> String {
        self.model_aliases()
            .remove(spec)
            .unwrap_or_else(|| spec.to_string())
    }

    fn validate(&self) -> eyre::Result<()> {
        let malformed = |s: &str| s.is_empty() || s.chars().any(char::is_whitespace);
        if let Some((alias, id)) = self
            .models
            .iter()
            .find(|(alias, id)| malformed(alias) || malformed(id))
        {
            return Err(eyre::eyre!(
                "[coding_session.codex.models] {alias:?} = {id:?}: names and ids are non-empty and carry no whitespace"
            ));
        }
        Ok(())
    }
}

/// Thinking-effort levels the Claude Code CLI accepts.
pub const CLAUDE_CODE_EFFORTS: &[&str] = &["low", "medium", "high", "max"];

/// Reasoning-effort levels the Codex CLI accepts.
pub const CODEX_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// The engines a coding round can run on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingEngineKind {
    /// The Claude Code CLI on the user's subscription token.
    #[default]
    ClaudeCode,
    /// The Claude Code CLI billed per token through Bedrock, on credentials
    /// the hub mints for the round (`[coding_session.bedrock]`).
    ClaudeCodeBedrock,
    /// The Codex CLI on the user's ChatGPT login (vault `codex/auth_json`).
    Codex,
    /// The Codex CLI billed per token on an OpenAI API key (vault
    /// `openai/api_key`).
    CodexApi,
    /// The hub's own agent loop on any model or tier the install can route,
    /// with file and shell tools over the round's sandbox.
    Model,
}

impl CodingEngineKind {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "claude_code" => Some(Self::ClaudeCode),
            "claude_code_bedrock" => Some(Self::ClaudeCodeBedrock),
            "codex" => Some(Self::Codex),
            "codex_api" => Some(Self::CodexApi),
            "model" => Some(Self::Model),
            _ => None,
        }
    }

    pub fn is_bedrock(self) -> bool {
        matches!(self, Self::ClaudeCodeBedrock)
    }

    /// Whether the engine runs on a person's subscription login rather than
    /// a metered key or the hub's own providers.
    pub fn is_subscription(self) -> bool {
        matches!(self, Self::ClaudeCode | Self::Codex)
    }

    /// The effort levels the engine's CLI accepts.
    pub fn efforts(self) -> &'static [&'static str] {
        match self {
            Self::Codex | Self::CodexApi => CODEX_EFFORTS,
            _ => CLAUDE_CODE_EFFORTS,
        }
    }
}

/// What makes a round move on to the next entry of the fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackTrigger {
    /// The engine cannot serve this user at all (no token for it).
    NoToken,
    /// The round was refused for rate or usage limits.
    RateLimited,
    /// The round failed without doing usable work.
    Failure,
}

/// One entry of a fallback chain: an engine, optionally pinned to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineEntry {
    pub kind: CodingEngineKind,
    pub model: Option<String>,
}

impl EngineEntry {
    /// `engine` or `engine:model`, where `engine` names a
    /// [`CodingEngineKind`]; a spec that names no engine is a model or tier
    /// for the hub loop (`openrouter:anthropic:cheap`, `bedrock:anthropic:standard`,
    /// a concrete id), the grammar `[llm].failover` uses.
    pub fn parse(spec: &str) -> eyre::Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(eyre::eyre!("fallback entry {spec:?} is empty"));
        }
        let (name, model) = match spec.split_once(':') {
            Some((name, model)) => (name, Some(model.trim())),
            None => (spec, None),
        };
        let (kind, model) = match CodingEngineKind::parse(name) {
            Some(kind) => (kind, model),
            None => (CodingEngineKind::Model, Some(spec)),
        };
        match model {
            Some("") => Err(eyre::eyre!("fallback entry {spec:?} has an empty model")),
            model => Ok(Self {
                kind,
                model: model.map(str::to_string),
            }),
        }
    }

    /// Every entry of a chain, in order.
    pub fn parse_all(specs: &[String]) -> eyre::Result<Vec<Self>> {
        specs.iter().map(|spec| Self::parse(spec)).collect()
    }
}

/// `[coding_session.engine]` — the primary engine and its fallback chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingEngineConfig {
    #[serde(default)]
    pub kind: CodingEngineKind,
    /// Tried in order when the primary cannot run or fails, each as
    /// `engine[:model]`; a model here overrides the round's own.
    #[serde(default)]
    pub fallback: Vec<String>,
    /// Whether the chain may use an engine that bills per token.
    #[serde(default)]
    pub allow_metered: bool,
    #[serde(default = "default_fallback_on")]
    pub fallback_on: Vec<FallbackTrigger>,
}

impl Default for CodingEngineConfig {
    fn default() -> Self {
        Self {
            kind: CodingEngineKind::default(),
            fallback: Vec::new(),
            allow_metered: false,
            fallback_on: default_fallback_on(),
        }
    }
}

impl CodingEngineConfig {
    /// The primary and every fallback engine, in chain order; a chain that
    /// does not parse counts as the primary alone.
    pub fn kinds(&self) -> Vec<CodingEngineKind> {
        std::iter::once(self.kind)
            .chain(
                EngineEntry::parse_all(&self.fallback)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|e| e.kind),
            )
            .collect()
    }

    /// Whether every engine in the chain runs on a person's subscription
    /// login, so nothing can run until one is installed.
    pub fn subscription_only(&self) -> bool {
        self.kinds().iter().all(|kind| kind.is_subscription())
    }
}

fn default_fallback_on() -> Vec<FallbackTrigger> {
    vec![
        FallbackTrigger::NoToken,
        FallbackTrigger::RateLimited,
        FallbackTrigger::Failure,
    ]
}

const DEFAULT_FANOUT_MAX_PARALLEL: usize = 4;

/// Turns allowed in one coding round. Sized so the wall clock, not the turn
/// count, is what stops an ordinary round: production measured ~9s per turn, so
/// a 30-turn cap bound at roughly a quarter of a 20-minute budget and cut real
/// work short — the failure this default was raised to fix.
pub const DEFAULT_DRIVER_MAX_TURNS: u32 = 80;

/// Wall clock for one coding round, across all of its attempts.
pub const DEFAULT_DRIVER_TIMEOUT_SECS: u64 = 20 * 60;

/// Floor for `timeout_secs`: below this a round cannot get past provisioning,
/// and `run_with_retry` would have no budget left to work with.
pub const MIN_DRIVER_TIMEOUT_SECS: u64 = 120;

impl CodingSessionConfig {
    pub fn effective_exposed_tools(&self) -> Vec<String> {
        patterns_or_default(&self.exposed_tools, DEFAULT_CODING_SESSION_EXPOSED_TOOLS)
    }

    /// Concurrency cap for a `coding.fanout` wave; `0` (unset) → the default.
    pub fn effective_fanout_max_parallel(&self) -> usize {
        match self.fanout_max_parallel {
            0 => DEFAULT_FANOUT_MAX_PARALLEL,
            n => n as usize,
        }
    }

    /// Turn cap for one coding round; `0` (unset) → the default.
    pub fn effective_max_turns(&self) -> u32 {
        match self.max_turns {
            0 => DEFAULT_DRIVER_MAX_TURNS,
            n => n,
        }
    }

    /// Wall clock for one coding round; `0` (unset) → the default.
    pub fn effective_timeout(&self) -> Duration {
        Duration::from_secs(match self.timeout_secs {
            0 => DEFAULT_DRIVER_TIMEOUT_SECS,
            n => n,
        })
    }

    /// `cache_dir` as a path, if configured.
    pub fn cache_dir_path(&self) -> Option<&std::path::Path> {
        self.cache_dir.as_deref().map(std::path::Path::new)
    }
}

/// Where a coding round runs. `local` spawns the `claude` subprocess in the
/// hub container (the default); `remote` prepares a scope on the `[exec.remote]`
/// worker and runs it there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    #[default]
    Local,
    Remote,
}

/// Selects the coding execution backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorConfig {
    #[serde(default)]
    pub kind: ExecutorKind,
    /// How long the worker keeps an idle remote scope before reclaiming it.
    /// Each round renews it. Default: two hours.
    #[serde(default = "default_scope_ttl_secs")]
    pub scope_ttl_secs: u64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            kind: ExecutorKind::Local,
            scope_ttl_secs: default_scope_ttl_secs(),
        }
    }
}

fn default_scope_ttl_secs() -> u64 {
    2 * 60 * 60
}

/// How the bot decides which messages to respond to in channels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseMode {
    /// Respond to all messages in allowed channels.
    All,
    /// Only respond when @mentioned, in DMs, or in active threads.
    #[default]
    MentionOnly,
    /// Quiet mode: only react with emoji, never send text responses.
    /// Useful for observation/monitoring channels.
    Quiet,
}

// ─── Group Chat Configuration ────────────────────────────────────────────────

/// How the agent decides whether to activate in a group channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Respond to every message in the channel.
    Always,
    /// Only respond when explicitly @mentioned or in an active thread.
    #[default]
    Mention,
    /// Use heuristics: mentioned, asked a question, relevant topic, or active thread.
    Smart,
    /// Observe only — never send text responses (useful for monitoring/learning).
    Quiet,
}

/// How the bot delivers its response in a channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupResponseMode {
    /// Always reply in a thread.
    Reply,
    /// Always reply inline in the channel.
    Inline,
    /// If the incoming message was in a thread, reply in that thread;
    /// otherwise reply inline.
    #[default]
    Auto,
}

/// Top-level `[groups]` configuration section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupsConfig {
    /// Default activation mode for channels without an explicit override.
    #[serde(default)]
    pub default_activation: ActivationMode,
    /// Default response delivery mode.
    #[serde(default)]
    pub default_response_mode: GroupResponseMode,
    /// Per-channel configuration entries.
    #[serde(default)]
    pub channels: Vec<GroupChannelConfig>,
}

/// Configuration for a single channel/group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupChannelConfig {
    /// Channel/group ID (e.g., Slack channel ID like "C0FAKECHAN4").
    pub id: String,
    /// Activation mode override for this channel.
    #[serde(default)]
    pub activation: Option<ActivationMode>,
    /// Response mode override for this channel.
    #[serde(default)]
    pub response_mode: Option<GroupResponseMode>,
    /// Bind this channel to a specific agent ID.
    #[serde(default = "default_agent_binding")]
    pub agent: String,
    /// Whether to use thread-isolated sessions (each thread gets its own session context).
    #[serde(default = "default_thread_isolation")]
    pub thread_isolation: bool,
    /// Activation keywords — agent responds when any of these appear, even without @mention.
    #[serde(default)]
    pub keywords: Vec<String>,
}

fn default_agent_binding() -> String {
    "default".to_string()
}

fn default_thread_isolation() -> bool {
    true
}

impl GroupsConfig {
    /// Look up configuration for a specific channel, falling back to defaults.
    pub fn for_channel(&self, channel_id: &str) -> ResolvedGroupConfig {
        let channel_config = self.channels.iter().find(|c| c.id == channel_id);

        ResolvedGroupConfig {
            activation: channel_config
                .and_then(|c| c.activation.clone())
                .unwrap_or_else(|| self.default_activation.clone()),
            response_mode: channel_config
                .and_then(|c| c.response_mode.clone())
                .unwrap_or_else(|| self.default_response_mode.clone()),
            agent: channel_config
                .map(|c| c.agent.clone())
                .unwrap_or_else(|| "default".to_string()),
            thread_isolation: channel_config.map(|c| c.thread_isolation).unwrap_or(true),
            keywords: channel_config
                .map(|c| c.keywords.clone())
                .unwrap_or_default(),
            is_allowlisted: channel_config.is_some(),
        }
    }

    /// Check if a channel is explicitly configured (in the allowlist).
    pub fn is_channel_configured(&self, channel_id: &str) -> bool {
        self.channels.iter().any(|c| c.id == channel_id)
    }

    /// Get the list of all configured channel IDs.
    pub fn configured_channel_ids(&self) -> Vec<&str> {
        self.channels.iter().map(|c| c.id.as_str()).collect()
    }
}

/// Fully resolved configuration for a channel after applying defaults.
#[derive(Debug, Clone)]
pub struct ResolvedGroupConfig {
    /// Effective activation mode.
    pub activation: ActivationMode,
    /// Effective response delivery mode.
    pub response_mode: GroupResponseMode,
    /// Bound agent ID.
    pub agent: String,
    /// Whether thread isolation is enabled.
    pub thread_isolation: bool,
    /// Activation keywords.
    pub keywords: Vec<String>,
    /// Whether this channel was explicitly configured (vs. using defaults).
    pub is_allowlisted: bool,
}

/// Top-level skills system configuration.
///
/// ```toml
/// [skills]
/// skills_dir = "./data/skills"
/// enabled = ["slack-integration"]
/// ```
/// Keeping the native-query tool schema stocked with the real table and column
/// names of each Metabase database, so the model writes SQL from the schema
/// rather than from a guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetabaseSyncConfig {
    /// Run the background pass. Off leaves the in-request learning untouched.
    #[serde(default = "default_true")]
    pub schema_sync: bool,
    #[serde(default = "default_metabase_sync_interval_secs")]
    pub sync_interval_secs: u64,
    #[serde(default = "default_metabase_tables_with_columns")]
    pub tables_with_columns: usize,
    #[serde(default = "default_metabase_max_requests_per_pass")]
    pub max_requests_per_pass: usize,
}

fn default_metabase_sync_interval_secs() -> u64 {
    86_400
}

/// Measured on a live install, the tables carrying column mistakes are heavily
/// concentrated while the mistaken names are a long tail; a couple of dozen
/// covered the large majority. Which tables fill the slots is learned per
/// install from usage — only the count is a default.
fn default_metabase_tables_with_columns() -> usize {
    25
}

/// Enough to reach every database on a large install rather than stopping
/// part-way through the list.
///
/// Measured live: 34 databases at 25 described tables each needs ~900, and a
/// 200-request budget reached only the first eight. The pass now visits
/// databases in order of need, so a budget below the total degrades by
/// describing fewer of the unimportant ones rather than missing whole
/// databases — but the ceiling should still clear an ordinary install.
fn default_metabase_max_requests_per_pass() -> usize {
    1_000
}

impl Default for MetabaseSyncConfig {
    fn default() -> Self {
        Self {
            schema_sync: true,
            sync_interval_secs: default_metabase_sync_interval_secs(),
            tables_with_columns: default_metabase_tables_with_columns(),
            max_requests_per_pass: default_metabase_max_requests_per_pass(),
        }
    }
}

impl MetabaseSyncConfig {
    /// The pass interval, floored so a zero cannot turn the loop into a
    /// busy-wait.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.sync_interval_secs.max(60))
    }

    /// Per-pass bounds, floored so a zero cannot silently disable the work the
    /// pass exists to do.
    pub fn caps(&self) -> (usize, usize) {
        let tables = self.tables_with_columns.max(1);
        (tables, self.max_requests_per_pass.max(tables + 1))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Directory containing skill subdirectories.
    /// Defaults to `"{data_dir}/skills"`.
    #[serde(default)]
    pub skills_dir: Option<String>,
    /// Explicitly enabled skill names. If empty, all discovered skills are eligible.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// WASM-skill-specific policy. See [`WasmSkillsConfig`].
    #[serde(default)]
    pub wasm: WasmSkillsConfig,
    /// Metabase schema sync. See [`MetabaseSyncConfig`].
    #[serde(default)]
    pub metabase: MetabaseSyncConfig,
    /// Auto-load every built-in skill whose shared credentials are already
    /// present at hub startup, so a redeploy doesn't drop previously-enabled
    /// skills (the in-memory `SkillStateTracker` is rebuilt empty on boot).
    /// Per-user / OAuth-managed skills stay on-demand. Default `true`; set
    /// `false` to keep the leaner DW-277 deferred-loading behaviour.
    #[serde(default = "default_true")]
    pub auto_load: bool,
    /// Remote skill catalogs (curated Git repos) this install can list and
    /// install skills from. Empty by default — the capability ships in the
    /// binary; each install opts in by pointing at one or more catalogs.
    #[serde(default)]
    pub catalogs: Vec<CatalogSource>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            skills_dir: None,
            enabled: Vec::new(),
            wasm: WasmSkillsConfig::default(),
            metabase: MetabaseSyncConfig::default(),
            auto_load: true,
            catalogs: Vec::new(),
        }
    }
}

/// A curated Git repo that lists installable Workforce skills (external
/// `SKILL.md` and compiled WASM) via a JSON manifest. The install browses it
/// and pulls skills on demand. A token for reading a private catalog is
/// resolved from the vault at `skills/catalog/<name>/token` — a dedicated org
/// credential, deliberately separate from `skills/github/*` (users' own GitHub
/// tokens) — keyed by this catalog's `name`, not stored here.
///
/// ```toml
/// [[skills.catalogs]]
/// name = "example"
/// repo = "https://github.com/acme/skills"
/// ref = "main"
/// manifest = "workforce/catalog.json"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSource {
    /// Short identifier used to select this catalog (e.g. "example").
    pub name: String,
    /// GitHub repository URL or `owner/repo` reference.
    pub repo: String,
    /// Branch, tag, or full 40-hex commit to read. Defaults to `main`.
    #[serde(rename = "ref", default = "default_catalog_ref")]
    pub git_ref: String,
    /// Repo-relative path of the catalog manifest.
    #[serde(default = "default_catalog_manifest")]
    pub manifest: String,
}

fn default_catalog_ref() -> String {
    "main".to_string()
}

fn default_catalog_manifest() -> String {
    "workforce/catalog.json".to_string()
}

#[cfg(test)]
mod catalog_source_tests {
    use super::*;

    #[test]
    fn catalog_source_applies_defaults() {
        let src: CatalogSource =
            toml::from_str("name = \"example\"\nrepo = \"https://github.com/o/r\"\n").unwrap();
        assert_eq!(src.git_ref, "main");
        assert_eq!(src.manifest, "workforce/catalog.json");
    }

    #[test]
    fn skills_config_parses_catalogs_block() {
        let cfg: SkillsConfig =
            toml::from_str("[[catalogs]]\nname = \"example\"\nrepo = \"o/r\"\nref = \"dev\"\n")
                .unwrap();
        assert_eq!(cfg.catalogs.len(), 1);
        assert_eq!(cfg.catalogs[0].git_ref, "dev");
        assert_eq!(cfg.catalogs[0].manifest, "workforce/catalog.json");
    }
}

/// The install's `[environments]`: a kill switch over every agent's own
/// environments. `enabled = false` turns them off everywhere; absent or `true`
/// leaves each agent's `config.toml` to decide.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvironmentsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl EnvironmentsConfig {
    pub fn allowed(&self) -> bool {
        self.enabled != Some(false)
    }
}

/// An agent's own `[environments]`: named bundles that pin its multi-instance
/// skills and MCP workspaces, and the one active when a session names none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentEnvironmentsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defs: BTreeMap<String, EnvironmentDef>,
}

/// One environment. A skill binds to an ordered set of its instances, the first
/// being the one used when a call names none; an MCP workspace to one variant.
/// A draft, which is what the agent itself makes, is never active until a
/// person activates it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, String>,
}

/// Words that clear a session's or an agent's environment choice, so never
/// the name of one.
pub const ENVIRONMENT_CLEAR_WORDS: [&str; 5] = ["default", "reset", "off", "clear", "none"];

impl AgentEnvironmentsConfig {
    pub fn validate(&self) -> eyre::Result<()> {
        if let Some(default) = &self.default {
            let def = self.defs.get(default).ok_or_else(|| {
                eyre::eyre!(
                    "environments.default names `{default}`, which is not an environment of this agent"
                )
            })?;
            eyre::ensure!(
                !def.draft,
                "environments.default names `{default}`, which is still a draft"
            );
        }
        self.defs
            .iter()
            .try_for_each(|(name, def)| def.validate(name))
    }
}

impl EnvironmentDef {
    fn validate(&self, name: &str) -> eyre::Result<()> {
        eyre::ensure!(
            !name.trim().is_empty(),
            "an environment name must not be blank"
        );
        eyre::ensure!(
            !ENVIRONMENT_CLEAR_WORDS.contains(&name),
            "`{name}` clears an environment choice, so it cannot name one"
        );
        self.skills.iter().try_for_each(|(skill, instances)| {
            eyre::ensure!(
                !skill.trim().is_empty(),
                "environment `{name}` binds a blank skill name"
            );
            eyre::ensure!(
                !instances.is_empty(),
                "environment `{name}` binds `{skill}` to no instance"
            );
            eyre::ensure!(
                instances.iter().all(|i| !i.trim().is_empty()),
                "environment `{name}` binds `{skill}` to a blank instance"
            );
            let unique: std::collections::BTreeSet<&String> = instances.iter().collect();
            eyre::ensure!(
                unique.len() == instances.len(),
                "environment `{name}` lists an instance of `{skill}` twice"
            );
            Ok(())
        })?;
        self.mcp.iter().try_for_each(|(workspace, variant)| {
            eyre::ensure!(
                !workspace.trim().is_empty() && !variant.trim().is_empty(),
                "environment `{name}` has a blank MCP workspace or variant"
            );
            Ok(())
        })
    }
}

/// `[worker_read_sources]` — gates the `read_source.*` tools that let a
/// read-only worker sub-agent acquire additional read-only surfaces on demand.
/// Off by default: enabling widens what an autonomous worker can read (still
/// read-only, still per-user-scoped), so it is an explicit operator decision.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerReadSourcesConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// Policy for WASM skill installation, separate from the general skills
/// directory layout. Cloud deployments tighten these; single-tenant CLI
/// keeps the defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmSkillsConfig {
    /// When `true`, the `wasm_skill.install` tool's `project_path`
    /// branch is allowed — it invokes `cargo build
    /// --target wasm32-unknown-unknown` against arbitrary input. Set to
    /// `false` in cloud / multi-tenant deployments where the agent's
    /// credentials shouldn't be able to drive arbitrary build scripts.
    /// Default: `true` (single-tenant dev convenience).
    #[serde(default = "default_true")]
    pub allow_build_on_install: bool,
    /// When `true`, every install must include a `sha256` pin. Cloud
    /// deployments should set this so an attacker who substitutes a
    /// `.wasm` at the source can't get it activated. Default: `false`
    /// (back-compat with existing flows that just record the digest).
    #[serde(default)]
    pub require_sha256_pin: bool,
}

impl Default for WasmSkillsConfig {
    fn default() -> Self {
        Self {
            allow_build_on_install: true,
            require_sha256_pin: false,
        }
    }
}

impl SkillsConfig {
    /// Resolve the effective skills directory, using `data_dir` as the base if not explicitly set.
    pub fn resolved_skills_dir(&self, data_dir: &str) -> std::path::PathBuf {
        match &self.skills_dir {
            Some(dir) => std::path::PathBuf::from(dir),
            None => std::path::PathBuf::from(data_dir).join("skills"),
        }
    }
}

/// GitHub integration configuration.
///
/// ```toml
/// [github]
/// # token = "ghp_..."  # Or set GITHUB_TOKEN env var
/// # default_owner = "your-org"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubConfig {
    /// GitHub Personal Access Token.
    /// Falls back to `GITHUB_TOKEN` env var if not set.
    #[serde(default)]
    pub token: Option<String>,
    /// GitHub API base URL. Defaults to `https://api.github.com`.
    /// Set to your GitHub Enterprise URL if needed.
    #[serde(default = "default_github_base_url")]
    pub base_url: String,
    /// Default owner (org or user) when omitted from tool calls.
    #[serde(default)]
    pub default_owner: Option<String>,
}

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            token: None,
            base_url: default_github_base_url(),
            default_owner: None,
        }
    }
}

fn default_github_base_url() -> String {
    "https://api.github.com".to_string()
}

/// Secrets vault configuration.
///
/// ```toml
/// [vault]
/// enabled = true
/// # path = "./data/vault.enc"       # default: {data_dir}/vault.enc
/// # key_file = "./data/vault-key.txt" # default: {data_dir}/vault-key.txt
/// # backend = "file"                  # or "postgres"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Whether the vault is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Path to the encrypted vault file. Defaults to `"{data_dir}/vault.enc"`.
    #[serde(default)]
    pub path: Option<String>,
    /// Path to the master key file. Defaults to `"{data_dir}/vault-key.txt"`.
    /// Alternatively set `WORKFORCE_VAULT_KEY` env var.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Storage backend: `"file"` (default, one encrypted file per vault) or
    /// `"postgres"` (rows in the shared database, needed when more than one
    /// hub process serves the same vaults). Deliberately excluded from
    /// `POSTGRES_DEFAULT_STORES`: existing vaults move with
    /// `workforce vault migrate` first, so enabling is explicit.
    #[serde(default = "default_vault_backend")]
    pub backend: String,
    /// Postgres backend: how often each process polls for vault changes made
    /// by other processes, in milliseconds.
    #[serde(default = "default_vault_sync_interval_ms")]
    pub sync_interval_ms: u64,
    /// Postgres backend: vault audit records older than this are deleted.
    #[serde(default = "default_vault_audit_retention_days")]
    pub audit_retention_days: u32,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            key_file: None,
            backend: default_vault_backend(),
            sync_interval_ms: default_vault_sync_interval_ms(),
            audit_retention_days: default_vault_audit_retention_days(),
        }
    }
}

fn default_vault_backend() -> String {
    "file".to_string()
}

fn default_vault_sync_interval_ms() -> u64 {
    1000
}

fn default_vault_audit_retention_days() -> u32 {
    365
}

/// Trait for resolving vault references in config files.
///
/// Implemented by the `Vault` in the hub crate. Used by `config_loader`
/// to expand `${vault:path}` references in skill config files.
pub trait VaultResolver: Send + Sync {
    /// Resolve a vault path to its secret value.
    fn resolve(&self, path: &str) -> Option<String>;
}

/// A single vault entry — value plus per-entry audit metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultEntry {
    pub value: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Tagged actor string: e.g. `user:<uuid>`, `agent:<id>`, `system:<job>`,
    /// `cli`, `rest:owner-token`, `legacy`.
    pub created_by: String,
    pub updated_by: String,
}

/// Who is performing a vault write. Recorded as `created_by`/`updated_by`
/// on the resulting [`VaultEntry`].
#[derive(Debug, Clone, Copy)]
pub enum VaultActor<'a> {
    /// A real end-user — `&str` is the canonical identity UUID.
    User(&'a str),
    /// An agent process — `&str` is the agent id.
    Agent(&'a str),
    /// A hub-internal system job — `&str` is the job name (e.g. `slack_token_sync`).
    System(&'a str),
    /// The `workforce vault set` CLI on the hub host.
    Cli,
    /// REST `/api/vault/*` endpoint authenticated with the owner token.
    RestOwner,
    /// Synthetic actor used when migrating a v1 vault file forward.
    Legacy,
}

impl VaultActor<'_> {
    /// Render as the tagged string stored in [`VaultEntry::created_by`].
    pub fn audit_string(&self) -> String {
        match self {
            VaultActor::User(uid) => format!("user:{uid}"),
            VaultActor::Agent(id) => format!("agent:{id}"),
            VaultActor::System(job) => format!("system:{job}"),
            VaultActor::Cli => "cli".to_string(),
            VaultActor::RestOwner => "rest:owner-token".to_string(),
            VaultActor::Legacy => "legacy".to_string(),
        }
    }
}

/// Which tier of a layered vault produced a value. The four tiers form a
/// resolution cascade from most-specific (T1) to most-general (T4); writes
/// pick a tier explicitly.
///
/// Serde aliases preserve the pre-2026-05 labels so existing audit log
/// entries still parse after the rename.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultSource {
    /// T1: `agent.get("users/{uid}/{path}")` — one user's creds on one agent.
    #[serde(alias = "agent_user_scoped")]
    AgentPerUser,
    /// T2: `global.get("users/{uid}/{path}")` — one user's creds on any agent.
    #[serde(alias = "global_user_scoped")]
    GlobalPerUser,
    /// T3: `agent.get(path)` — shared across all users of one agent.
    #[serde(alias = "agent_shared_bare")]
    AgentSpecific,
    /// T4: `global.get(path)` — shared across all users and all agents.
    #[serde(alias = "global_shared_bare")]
    Global,
    /// Returned by single-vault impls that don't track layering.
    Unknown,
}

impl VaultSource {
    /// Lower-snake-case label for CLI / agent tool output.
    pub fn label(&self) -> &'static str {
        match self {
            VaultSource::AgentPerUser => "agent_per_user",
            VaultSource::GlobalPerUser => "global_per_user",
            VaultSource::AgentSpecific => "agent_specific",
            VaultSource::Global => "global",
            VaultSource::Unknown => "unknown",
        }
    }

    /// Resolution rank — lower wins. `Unknown` sorts last so it never beats a
    /// real tier in shadow detection.
    pub fn rank(&self) -> u8 {
        match self {
            VaultSource::AgentPerUser => 1,
            VaultSource::GlobalPerUser => 2,
            VaultSource::AgentSpecific => 3,
            VaultSource::Global => 4,
            VaultSource::Unknown => u8::MAX,
        }
    }

    /// The four addressable tiers in resolution order. Excludes `Unknown`.
    pub fn all() -> [VaultSource; 4] {
        [
            VaultSource::AgentPerUser,
            VaultSource::GlobalPerUser,
            VaultSource::AgentSpecific,
            VaultSource::Global,
        ]
    }

    /// Whether this tier is per-user (the path is keyed by identity uuid).
    pub fn is_per_user(&self) -> bool {
        matches!(self, VaultSource::AgentPerUser | VaultSource::GlobalPerUser)
    }

    /// Whether writes to this tier require an admin role.
    pub fn requires_admin_to_write(&self) -> bool {
        matches!(self, VaultSource::Global)
    }
}

/// Read/write secret storage backend.
///
/// Lives in `workforce-common` so skills (which cannot depend on `hub`) can
/// accept `Arc<dyn SecretStore>` and persist rotating credentials like OAuth
/// refresh tokens at call time. `Vault` in the hub crate implements this.
pub trait SecretStore: Send + Sync {
    /// Read a secret by dot-notation path.
    fn get(&self, path: &str) -> Option<String>;

    /// Write a secret. Re-encrypts and persists to disk.
    fn set(&self, path: &str, value: &str) -> eyre::Result<()>;

    /// Delete a secret. Returns whether the key existed.
    fn delete(&self, path: &str) -> eyre::Result<bool>;

    /// List secret paths, optionally filtered by prefix. Never returns values.
    fn list(&self, prefix: Option<&str>) -> Vec<String>;

    /// Read a secret only from user-scoped layers, ignoring shared/agent
    /// fallbacks. Returns `None` when no user identity is active or no
    /// user-specific override exists for this path.
    fn get_user_override(&self, _path: &str) -> Option<String> {
        None
    }

    /// Write a secret recording the given actor in audit metadata. Default
    /// impl forwards to [`SecretStore::set`] for backends that don't track
    /// metadata; concrete vault implementations override this.
    fn set_with_actor(&self, path: &str, value: &str, _actor: VaultActor<'_>) -> eyre::Result<()> {
        self.set(path, value)
    }

    /// Read a secret with its audit metadata. Default impl returns `None`
    /// for backends that don't track metadata.
    fn get_entry(&self, _path: &str) -> Option<VaultEntry> {
        None
    }

    /// Read a secret along with the layered tier it came from. Default impl
    /// tags the result `Unknown`; layered stores override this to surface
    /// which tier resolved.
    fn get_with_source(&self, path: &str) -> Option<(String, VaultSource)> {
        self.get(path).map(|v| (v, VaultSource::Unknown))
    }

    /// Walk every tier of a layered store and return all values present at
    /// `path`, in resolution order. Single-vault impls return at most one
    /// entry. Used to detect shadowing.
    fn inspect(&self, _path: &str) -> Vec<(VaultSource, String)> {
        Vec::new()
    }

    /// Read a secret from EXACTLY one tier — single-tier pinning, with no
    /// precedence fallthrough to other tiers. Per-user tiers resolve under the
    /// active identity; `None` when nothing exists at that tier. Backed by
    /// [`SecretStore::inspect`], so a layered store pins precisely while a
    /// single-vault store (which doesn't track tiers) returns `None`.
    fn get_at_tier(&self, path: &str, tier: VaultSource) -> Option<String> {
        self.inspect(path)
            .into_iter()
            .find(|(t, _)| *t == tier)
            .map(|(_, v)| v)
    }

    /// Write to an explicit tier. `LayeredUserVault` implements this fully;
    /// single-vault impls reject every tier they cannot address.
    fn set_at_tier(
        &self,
        _path: &str,
        _value: &str,
        tier: VaultSource,
        _actor: VaultActor<'_>,
    ) -> eyre::Result<()> {
        Err(eyre::eyre!(
            "this secret store cannot address tier `{}`",
            tier.label()
        ))
    }

    /// Delete from an explicit tier. Default returns `Ok(false)` for any tier
    /// a single-vault impl cannot reach; layered stores override.
    fn delete_at_tier(&self, _path: &str, _tier: VaultSource) -> eyre::Result<bool> {
        Ok(false)
    }

    /// List `(path, entry)` pairs present at one explicit tier, optionally
    /// filtered by a tier-relative prefix. For per-user tiers the returned
    /// paths are the bare logical paths — the per-user `users/{uid}/` segment
    /// is stripped. Layered stores override; single-vault impls return empty.
    fn list_at_tier(&self, _prefix: Option<&str>, _tier: VaultSource) -> Vec<(String, VaultEntry)> {
        Vec::new()
    }

    /// Write several secrets as one unit. Stores that can commit them
    /// atomically override this; the default writes them in order.
    fn set_many(&self, entries: &[(&str, &str)], actor: VaultActor<'_>) -> eyre::Result<()> {
        entries
            .iter()
            .try_for_each(|(path, value)| self.set_with_actor(path, value, actor))
    }

    /// Pick up changes other processes made to the backing store. A no-op
    /// for stores only this process writes.
    fn sync(&self) -> eyre::Result<()> {
        Ok(())
    }

    /// The concrete location a write to `path` lands on. Keys
    /// [`SecretStore::refresh_lock`] so every writer of one stored secret
    /// takes the same lock.
    fn write_location(&self, path: &str) -> String {
        path.to_string()
    }

    /// Lock serialising refresh cycles on this store's secrets across every
    /// process that shares the store. Defaults to a process-wide lock.
    fn refresh_lock(&self) -> std::sync::Arc<dyn crate::secret_lock::RefreshLock> {
        crate::secret_lock::LocalRefreshLock::shared()
    }

    /// Move `value` between two tier-addressed slots, as one unit where the
    /// store can. The destination is written before the source is removed,
    /// so a failure in between leaves a copy rather than a loss.
    fn move_at_tier(
        &self,
        from: (&str, VaultSource),
        to: (&str, VaultSource),
        value: &str,
        actor: VaultActor<'_>,
    ) -> eyre::Result<()> {
        self.set_at_tier(to.0, value, to.1, actor)?;
        self.delete_at_tier(from.0, from.1).map(drop)
    }
}

/// Per-channel configuration overrides for Slack.
///
/// ```toml
/// [slack.channel_overrides.C0FAKECHAN5]
/// system_prompt = "This is #fabrikam-dev — focused on Fabrikam CRM development."
/// response_mode = "all"
/// display_name = "#fabrikam-dev"
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackChannelOverride {
    /// System prompt injected into agent context for messages from this channel.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Override the response mode for this channel.
    #[serde(default)]
    pub response_mode: Option<ResponseMode>,
    /// Friendly display name for this channel in agent context.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Override activation keywords for this channel.
    #[serde(default)]
    pub activation_keywords: Option<Vec<String>>,
    /// Optional pre-agent topic gate. Presence enables the filter; the agent
    /// only sees messages classified as on-topic. See `ScopeFilterConfig`.
    #[serde(default)]
    pub scope: Option<ScopeFilterConfig>,
}

/// Per-channel pre-agent scope filter. Each inbound message is classified
/// against `persona`/`custom_prompt`; off-topic messages are short-circuited
/// with `canned_response` before the agent runs.
///
/// ```toml
/// [slack.channel_overrides.C_TRIAGE.scope]
/// persona = "triage"
/// mode = "reject"
/// canned_response = "I'm the triage bot. Please file bug reports here."
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScopeFilterConfig {
    /// Preset persona name (resolved by the Slack adapter's classifier).
    /// Recognised values include `triage`, `faq`, `qa`, `support`, `sales`.
    #[serde(default)]
    pub persona: Option<String>,
    /// Free-form topic definition. Overrides `persona` when both are set.
    #[serde(default)]
    pub custom_prompt: Option<String>,
    /// Action on off-topic messages: `reject` drops them; `warn` logs and
    /// passes through.
    #[serde(default)]
    pub mode: ScopeFilterMode,
    /// Canned response sent on reject. Falls back to a per-persona default.
    #[serde(default)]
    pub canned_response: Option<String>,
    /// Classifier model override (e.g. `claude-haiku-4-5`). Falls back to the
    /// provider's default model when unset.
    #[serde(default)]
    pub model: Option<String>,
    /// Who classifies: `llm` or `decision` (the install's Kev/Jev). Unset
    /// follows `[decision.uses] scope_gate`.
    #[serde(default)]
    pub classifier: Option<ScopeClassifier>,
    /// With the decision model: how sure it must be that a message is
    /// off-topic to reject it. Unset uses the install's
    /// `[decision.uses.scope_gate] min_probability`.
    #[serde(default)]
    pub min_probability: Option<Probability>,
}

/// Which classifier a scope filter runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeClassifier {
    Llm,
    Decision,
}

/// Behaviour when a message is classified off-topic.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeFilterMode {
    /// Short-circuit the message and reply with a canned response. (Default.)
    #[default]
    Reject,
    /// Log the off-topic classification but still pass the message to the agent.
    Warn,
}

/// Per-group configuration overrides.
///
/// Allows customising response mode, prompt, keywords, and agent binding
/// for specific channels/groups.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupOverride {
    /// Override the response mode for this group.
    #[serde(default)]
    pub response_mode: Option<ResponseMode>,
    /// Override the system prompt for this group (appended to identity).
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Activation keywords specific to this group.
    #[serde(default)]
    pub activation_keywords: Vec<String>,
    /// Bind this group to a specific agent ID.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Whether to use thread-isolated sessions for this group.
    #[serde(default)]
    pub thread_isolation: Option<bool>,
}

/// Configuration entry for a shared multi-agent channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedChannelEntry {
    /// The channel ID (e.g., Slack channel ID).
    pub channel_id: String,
    /// Agents in this channel: list of {agent_id, bot_user_id}.
    pub agents: Vec<SharedChannelAgent>,
    /// Default agent for unroutable messages.
    #[serde(default)]
    pub default_agent: Option<String>,
}

/// An agent participating in a shared channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedChannelAgent {
    /// Agent identifier.
    pub agent_id: String,
    /// Slack bot user ID for this agent.
    pub bot_user_id: String,
    /// Display name for plain-text @mention matching (e.g., "Brian").
    #[serde(default)]
    pub display_name: Option<String>,
}

fn default_gateway_listen() -> String {
    "0.0.0.0:8091".to_string()
}

/// Stores whose `backend` defaults to `"postgres"` when `[postgres] enabled = true`.
const POSTGRES_DEFAULT_STORES: &[&str] = &["telemetry", "identity", "pairing_store", "codegraph"];

/// When `[postgres] enabled = true`, fills in `backend = "postgres"` for any
/// store in [`POSTGRES_DEFAULT_STORES`] that didn't explicitly set one.
/// No-op otherwise. Operates on the parsed TOML before deserialisation.
fn apply_postgres_backend_defaults(table: &mut toml::Table) {
    let postgres_enabled = table
        .get("postgres")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !postgres_enabled {
        return;
    }
    for store in POSTGRES_DEFAULT_STORES {
        let store_entry = table
            .entry(store.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(store_table) = store_entry.as_table_mut() {
            store_table
                .entry("backend".to_string())
                .or_insert_with(|| toml::Value::String("postgres".to_string()));
        }
    }
}

impl WorkforceConfig {
    /// Load configuration from a TOML file, merging the runtime overlay
    /// (`{data_dir}/workforce.overlay.toml`, see [`crate::config_overlay`])
    /// over the infra-owned base at the raw-table level before env expansion.
    /// Validates after parse.
    ///
    /// An unreadable/unparsable overlay never takes the hub down: it is
    /// ignored with an error-level log (base-only boot) — but anything the
    /// overlay defined, e.g. UI-added agents, vanishes until it is fixed.
    pub fn load(path: &Path) -> eyre::Result<Self> {
        let raw = crate::config_overlay::load_raw(path)?;
        if let Some(err) = &raw.overlay_error {
            tracing::error!(
                overlay = %raw.overlay_path.display(),
                error = %err,
                "Config overlay unreadable — booting from the base config only; \
                 overlay-defined agents/settings are INACTIVE until the file is fixed"
            );
        }
        for path in &raw.stripped {
            tracing::error!(
                key_path = %path,
                overlay = %raw.overlay_path.display(),
                "Config overlay key is not overlayable (infra-owned) — ignored"
            );
        }
        let mut config = Self::from_raw_table(raw.merged())?;
        if let Some(config_dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            config.anchor_relative_paths(config_dir);
        }
        config.validate()?;
        Ok(config)
    }

    /// Resolve config paths that are meant to live beside the deployment
    /// (`hub.data_dir` and the vault file/key) against the config file's own
    /// directory when they are relative.
    ///
    /// A relative `data_dir = "./data"` otherwise resolves against the process
    /// CWD, which is fine for the hub (it runs from the app root) but wrong for
    /// any subprocess that loads the same `--config /app/workforce.toml` from a
    /// different CWD. `coding.session` spawns `mcp-serve` with its CWD set to a
    /// code worktree; `./data/vault.enc` then resolved to `<worktree>/data`,
    /// where `Vault::open` created a fresh EMPTY vault — so every per-user skill
    /// credential vanished and jira/github/datadog/… all failed to load. This
    /// makes the same config file resolve to the same absolute `data_dir`
    /// regardless of CWD. No-op when the paths are already absolute.
    fn anchor_relative_paths(&mut self, config_dir: &Path) {
        let anchor = |value: &mut String| {
            let p = Path::new(value.as_str());
            if p.is_relative() {
                *value = config_dir.join(p).to_string_lossy().into_owned();
            }
        };
        anchor(&mut self.hub.data_dir);
        if let Some(path) = self.vault.path.as_mut() {
            anchor(path);
        }
        if let Some(key_file) = self.vault.key_file.as_mut() {
            anchor(key_file);
        }
    }

    /// Parse a TOML string into a config (no overlay — used for embedded /
    /// test configs and by callers that already merged). Applies, in order:
    /// `${VAR}` env var expansion across every string value (see
    /// [`crate::config_expand`]), per-store backend defaulting when
    /// `[postgres] enabled = true`, and the legacy per-field env overrides in
    /// [`Self::apply_env_overrides`]. Vault (`${vault:...}`) references are
    /// resolved per-field at access time, not here.
    pub fn from_toml(toml_str: &str) -> eyre::Result<Self> {
        let table: toml::Table = toml_str
            .parse()
            .map_err(|e| eyre::eyre!("Failed to parse config: {}", e))?;
        Self::from_raw_table(table)
    }

    /// The shared post-parse pipeline: env expansion → postgres backend
    /// defaulting → deserialization → legacy env overrides. Public so the
    /// overlay writer validates a candidate merged table through the exact
    /// same path the next boot will take (write-accepts ⇔ boot-accepts).
    pub fn from_raw_table(mut table: toml::Table) -> eyre::Result<Self> {
        crate::config_expand::expand_env_vars_in_table(&mut table);
        apply_postgres_backend_defaults(&mut table);

        let mut config: Self = toml::Value::Table(table)
            .try_into()
            .map_err(|e| eyre::eyre!("Failed to deserialize config: {}", e))?;

        config.apply_env_overrides();
        Ok(config)
    }

    /// Apply environment variable overrides.
    fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_from(&|name| std::env::var(name));
    }

    /// [`Self::apply_env_overrides`], reading each variable through `var`,
    /// so tests can give their own environment without changing the
    /// process's, which other tests read at the same time.
    fn apply_env_overrides_from(
        &mut self,
        var: &dyn Fn(&str) -> Result<String, std::env::VarError>,
    ) {
        if let Ok(v) = var("WORKFORCE_NATS_URL") {
            self.nats.url = v;
        }
        if let Ok(v) = var("WORKFORCE_WEAVIATE_URL").or_else(|_| var("WEAVIATE_URL")) {
            self.weaviate.url = v;
        }
        if let Ok(v) = var("WORKFORCE_WEAVIATE_API_KEY").or_else(|_| var("WEAVIATE_API_KEY")) {
            self.weaviate.api_key = Some(v);
        }
        // Only the prefixed names: vellev's own CLI reads VELLEV_API_KEY as
        // an admin key, which must not reach the hub by accident.
        if let Ok(v) = var("WORKFORCE_VELLEV_URL") {
            self.vellev.url = v;
        }
        if let Ok(v) = var("WORKFORCE_VELLEV_API_KEY") {
            self.vellev.api_key = Some(v);
        }
        if let Ok(v) = var("WORKFORCE_VELLEV_TENANT") {
            self.vellev.tenant = v;
        }
        if let Ok(v) = var("WORKFORCE_LISTEN") {
            self.hub.listen = v;
        }
        if let Ok(v) = var("WORKFORCE_LOG_LEVEL") {
            self.hub.log_level = v;
        }
        // LLM API key: check provider-specific env vars first, then generic
        if self.llm.api_key.is_none() {
            let key = match self.llm.provider.as_str() {
                "openrouter" => var("OPENROUTER_API_KEY").ok(),
                "anthropic" => var("ANTHROPIC_API_KEY").ok(),
                "grok" | "xai" => var("XAI_API_KEY").ok(),
                _ => None,
            };
            if key.is_none() {
                // Fallback to generic
                self.llm.api_key = var("WORKFORCE_API_KEY").ok();
            } else {
                self.llm.api_key = key;
            }
        }
        if let Ok(v) = var("WORKFORCE_LLM_MODEL") {
            self.llm.default_model = v;
        }
        if let Ok(v) = var("WORKFORCE_LLM_PROVIDER") {
            self.llm.provider = v;
        }
        if let Ok(v) = var("WORKFORCE_TIMEZONE") {
            self.timezone = Some(v);
        }
        // Memory / embedding overrides
        if let Ok(v) = var("WORKFORCE_MEMORY_BACKEND") {
            self.memory.backend = v;
        }
        if let Ok(v) = var("WORKFORCE_ARTIFACTS_INDEX_BACKEND") {
            self.artifacts.index_backend = v;
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_VECTOR_BACKEND") {
            self.knowledge.vector_backend = v;
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_VECTOR_STANDBY") {
            self.knowledge.vector_standby = v;
        }
        if let Ok(v) = var("WORKFORCE_MEMORY_DB_PATH") {
            self.memory.db_path = v;
        }
        if self.memory.embedding_api_key.is_none() {
            self.memory.embedding_api_key = var("WORKFORCE_EMBEDDING_API_KEY")
                .or_else(|_| var("OPENAI_API_KEY"))
                .ok();
        }
        // GitHub token: env var fallback
        if self.github.token.is_none() {
            self.github.token = var("GITHUB_TOKEN").ok();
        }
        // Slack token env var overrides removed — Slack is now configured via
        // per-skill config at {skills_dir}/slack-integration/config.toml which
        // handles its own ${VAR} expansion.

        // Knowledge engine overrides
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_ENABLED") {
            self.knowledge.enabled = v.parse().unwrap_or(true);
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_ROOT") {
            self.knowledge.root = Some(v);
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_INDEX_ON_STARTUP") {
            self.knowledge.index_on_startup = v.parse().unwrap_or(false);
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_PARSE_CONCURRENCY") {
            self.knowledge.parse_concurrency = v.parse().unwrap_or(0);
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_SUMMARY_CONCURRENCY") {
            self.knowledge.summary_concurrency = v.parse().unwrap_or(0);
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_GENERATE_SUMMARIES") {
            self.knowledge.generate_summaries = v.parse().unwrap_or(false);
        }
        if let Ok(v) = var("SUMMARISER_URL").or_else(|_| var("WORKFORCE_KNOWLEDGE_OLLAMA_URL")) {
            self.knowledge.summariser.url = v;
        }
        if let Ok(v) = var("SUMMARISER_API_KEY").or_else(|_| var("OPENROUTER_API_KEY")) {
            self.knowledge.summariser.api_key = Some(v);
        }
        if let Ok(v) = var("SUMMARISER_PROVIDER") {
            self.knowledge.summariser.provider = v;
        }
        if let Ok(v) = var("WORKFORCE_KNOWLEDGE_SUMMARISER_MODEL") {
            self.knowledge.summariser.model = v;
        }

        if let Ok(v) = var("WORKFORCE_AWS_S3_ENABLED") {
            if let Some(b) = parse_bool_env("WORKFORCE_AWS_S3_ENABLED", &v) {
                self.aws_s3.enabled = b;
            }
        }
        if let Ok(v) = var("WORKFORCE_AWS_S3_BUCKET_ALLOWLIST") {
            if !v.trim().is_empty() {
                self.aws_s3.bucket_allowlist = parse_csv_env(&v);
            }
        }
        if let Ok(v) = var("WORKFORCE_AWS_S3_ASSUME_ROLE_ARN") {
            self.aws_s3.assume_role_arn = (!v.trim().is_empty()).then_some(v);
        }
        if let Ok(v) = var("WORKFORCE_AWS_S3_MAX_GET_BYTES") {
            match v.trim().parse::<u64>() {
                Ok(n) => self.aws_s3.max_get_bytes = n,
                Err(e) => tracing::warn!(
                    env = "WORKFORCE_AWS_S3_MAX_GET_BYTES",
                    value = %v,
                    error = %e,
                    "expected u64; keeping previous max_get_bytes",
                ),
            }
        }
        if let Ok(v) = var("WORKFORCE_AWS_S3_REQUIRE_APPROVAL_BUCKETS") {
            if !v.trim().is_empty() {
                self.aws_s3.require_approval_buckets = parse_csv_env(&v);
            }
        }
        if let Ok(v) = var("WORKFORCE_AWS_INFRA_ENABLED") {
            if let Some(b) = parse_bool_env("WORKFORCE_AWS_INFRA_ENABLED", &v) {
                self.aws_infra.enabled = b;
            }
        }
        if let Ok(v) = var("WORKFORCE_AWS_INFRA_ASSUME_ROLE_ARN") {
            self.aws_infra.assume_role_arn = (!v.trim().is_empty()).then_some(v);
        }
        if let Ok(v) = var("WORKFORCE_AWS_INFRA_REGION") {
            self.aws_infra.region = (!v.trim().is_empty()).then_some(v);
        }
    }

    /// Whether artifacts are indexed in vellev (`[artifacts] index_backend`).
    pub fn artifacts_indexed_in_vellev(&self) -> bool {
        self.artifacts.index_backend == VECTORS_ON_VELLEV
    }

    /// Whether code vectors are kept in vellev (`[knowledge] vector_backend`).
    pub fn code_vectors_in_vellev(&self) -> bool {
        self.knowledge.vector_backend == VECTORS_ON_VELLEV
    }

    /// Where memories, the artifact index and code vectors are kept, as
    /// health reports say it: each store's backend, or `"none"` for an
    /// index left on Weaviate while Weaviate is not enabled.
    pub fn store_backends(&self) -> [(&'static str, String); 3] {
        let index = |in_vellev: bool| match (in_vellev, self.weaviate.enabled) {
            (true, _) => VECTORS_ON_VELLEV,
            (false, true) => VECTORS_ON_WEAVIATE,
            (false, false) => "none",
        };
        [
            ("memories", self.memory.backend.clone()),
            (
                "artifact_index",
                index(self.artifacts_indexed_in_vellev()).to_string(),
            ),
            (
                "code_vectors",
                index(self.code_vectors_in_vellev()).to_string(),
            ),
        ]
    }

    /// The settings that leave an index nowhere while something else was
    /// moved to vellev: `"weaviate"`, the default, with Weaviate not
    /// enabled. Each with what then does not work, to be said loudly.
    pub fn indexes_left_nowhere(&self) -> Vec<String> {
        if self.weaviate.enabled || self.kept_in_vellev().is_none() {
            return Vec::new();
        }
        [
            (
                self.artifacts_indexed_in_vellev(),
                "[artifacts] index_backend",
                "artifacts are found by keyword only",
            ),
            (
                self.code_vectors_in_vellev(),
                "[knowledge] vector_backend",
                "code questions are answered without semantic search",
            ),
        ]
        .into_iter()
        .filter(|(in_vellev, _, _)| !in_vellev)
        .map(|(_, setting, cost)| {
            format!(
                "{setting} is \"weaviate\" and [weaviate] is not enabled, so {cost}: each store is chosen by its own setting, and moving another to vellev does not move this one. Set {setting} = \"vellev\" to keep it in vellev too"
            )
        })
        .collect()
    }

    /// The settings that shadow a store with vellev (`"shadow"`), while
    /// Weaviate serves it.
    pub fn shadowed(&self) -> Vec<&'static str> {
        [
            (self.memory.backend == SHADOWED, "[memory] backend"),
            (
                self.artifacts.index_backend == SHADOWED,
                "[artifacts] index_backend",
            ),
        ]
        .into_iter()
        .filter_map(|(chosen, setting)| chosen.then_some(setting))
        .collect()
    }

    /// The first setting that keeps something in vellev, if any does:
    /// memories, the artifact index and code vectors are each chosen apart.
    pub fn kept_in_vellev(&self) -> Option<&'static str> {
        [
            (self.memory.is_vellev(), "[memory] backend"),
            (
                self.artifacts_indexed_in_vellev(),
                "[artifacts] index_backend",
            ),
            (self.code_vectors_in_vellev(), "[knowledge] vector_backend"),
        ]
        .into_iter()
        .find_map(|(chosen, setting)| chosen.then_some(setting))
    }

    /// `[memory] backend`, checked where memories are opened
    /// (`resolve_memory_store`) and not when the config is read: a process
    /// that never opens the memory store is not stopped by a value only the
    /// memory store reads, as before shadow mode.
    pub fn check_memory_backend(&self) -> eyre::Result<()> {
        if !MEMORY_BACKENDS.contains(&self.memory.backend.as_str()) {
            return Err(eyre::eyre!(
                "[memory] backend must be one of {}, not \"{}\"",
                MEMORY_BACKENDS
                    .iter()
                    .filter(|b| !b.is_empty())
                    .map(|b| format!("\"{b}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.memory.backend
            ));
        }
        if self.memory.backend == SHADOWED {
            self.shadow_refusal("[memory] backend")?;
        }
        Ok(())
    }

    /// What a store set to `"shadow"` by `setting` needs, or `Err` naming
    /// the first thing missing.
    fn shadow_refusal(&self, setting: &'static str) -> eyre::Result<()> {
        self.vellev_reachable(&format!("{setting} = \"shadow\""))?;
        if !self.weaviate.enabled {
            return Err(eyre::eyre!(
                "{setting} = \"shadow\" needs [weaviate] enabled = true: Weaviate serves every read and write while vellev is shadowed"
            ));
        }
        if setting == "[memory] backend" && !self.embeddings.enabled() {
            return Err(eyre::eyre!(
                "[memory] backend = \"shadow\" needs [embeddings]: without the hub embedding memories, Weaviate and vellev would each embed with their own model, and the two could not be compared as the same model"
            ));
        }
        if !self.postgres.as_ref().is_some_and(|p| p.enabled) {
            return Err(eyre::eyre!(
                "{setting} = \"shadow\" needs [postgres] enabled = true: the two answers' comparisons are kept there"
            ));
        }
        // Both stores' shadows are built (DW-676: artifacts in PR 5,
        // memories in PR 8).
        Ok(())
    }

    /// The vellev key, address and tenant that `on_vellev` (a setting and
    /// its value) needs.
    fn vellev_reachable(&self, on_vellev: &str) -> eyre::Result<()> {
        if self.vellev.api_key.as_deref().is_none_or(str::is_empty) {
            return Err(eyre::eyre!(
                "{on_vellev} needs [vellev] api_key (or WORKFORCE_VELLEV_API_KEY)"
            ));
        }
        let url = self.vellev.url.trim();
        let host = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or_default();
        if host.is_empty() || host.starts_with('/') || url.contains(char::is_whitespace) {
            return Err(eyre::eyre!(
                "[vellev] url must be an http:// or https:// address of a vellev node, not \"{}\"",
                self.vellev.url
            ));
        }
        if self.vellev.tenant.trim().is_empty() {
            return Err(eyre::eyre!(
                "{on_vellev} needs [vellev] tenant (or WORKFORCE_VELLEV_TENANT): \
                 the tenant everything is kept in, which an operator creates on the node"
            ));
        }
        Ok(())
    }

    /// Validate the configuration.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.nats.url.is_empty() {
            return Err(eyre::eyre!("NATS URL cannot be empty"));
        }
        for (setting, waits) in [
            (
                "[artifacts] index_search_timeout_ms",
                self.artifacts.index_search_timeout_ms,
            ),
            (
                "[knowledge] vector_search_timeout_ms",
                self.knowledge.vector_search_timeout_ms,
            ),
        ] {
            if waits == 0 {
                return Err(eyre::eyre!(
                    "{setting} must be more than 0: it is how long a search waits for the vector index before it goes on without, and at 0 none would ever use it"
                ));
            }
        }
        if self.knowledge.vector_backend == SHADOWED {
            return Err(eyre::eyre!(
                "[knowledge] vector_backend cannot be \"shadow\": by the owner's decision of 2026-10-12 code vectors stay on Weaviate, unshadowed, until vellev's memory use for them can be cut (DW-676). Set it to \"weaviate\""
            ));
        }
        if self.artifacts.index_backend != SHADOWED
            && ![VECTORS_ON_WEAVIATE, VECTORS_ON_VELLEV]
                .contains(&self.artifacts.index_backend.as_str())
        {
            return Err(eyre::eyre!(
                "[artifacts] index_backend must be \"weaviate\" or \"vellev\", or \"shadow\" to have Weaviate serve while vellev is compared, not \"{}\"",
                self.artifacts.index_backend
            ));
        }
        if ![VECTORS_ON_WEAVIATE, VECTORS_ON_VELLEV]
            .contains(&self.knowledge.vector_backend.as_str())
        {
            return Err(eyre::eyre!(
                "[knowledge] vector_backend must be \"weaviate\" or \"vellev\", not \"{}\"",
                self.knowledge.vector_backend
            ));
        }
        for store in [ShadowedStore::Memories, ShadowedStore::Artifacts] {
            self.vellev.shadow.settings(store)?;
        }
        self.vellev.shadow.memory_reconcile()?;
        self.vellev.shadow.known_items()?;
        self.vellev.shadow.judge_cap()?;
        self.vellev.shadow.gates()?;
        self.vellev.shadow.self_first_gate()?;
        for (table, key) in self.vellev.shadow.unknown_keys() {
            tracing::warn!(
                table,
                key = %key,
                "{table} has no setting {key:?}; it is ignored: a newer hub's setting, or mistyped"
            );
        }
        if let Some(on_vellev) = self.kept_in_vellev() {
            self.vellev_reachable(&format!("{on_vellev} = \"vellev\""))?;
        }
        if self.artifacts.index_backend == SHADOWED {
            self.shadow_refusal("[artifacts] index_backend")?;
        }
        // JetStream caps replicas at 5; 0 would be rejected by the server.
        if !(1..=5).contains(&self.nats.stream_replicas) {
            return Err(eyre::eyre!(
                "[nats] stream_replicas must be 1-5, got {}",
                self.nats.stream_replicas
            ));
        }
        if self.llm.max_tokens == 0 {
            return Err(eyre::eyre!("LLM max_tokens must be > 0"));
        }
        if self.llm.max_rounds == 0 {
            return Err(eyre::eyre!("LLM max_rounds must be > 0"));
        }
        if self.llm.interactive_max_rounds == 0 {
            return Err(eyre::eyre!("LLM interactive_max_rounds must be > 0"));
        }
        if is_removed_provider(&self.llm.provider) {
            return Err(eyre::eyre!(
                "[llm] provider '{}' was removed: {REMOVED_PROVIDER_POINTER}",
                self.llm.provider
            ));
        }
        if self.llm.api_key.as_deref().is_some_and(is_claude_oat) {
            return Err(eyre::eyre!(oat_refusal("[llm] api_key")));
        }
        let coding_timeout = self.coding_session.timeout_secs;
        if coding_timeout != 0 && coding_timeout < MIN_DRIVER_TIMEOUT_SECS {
            return Err(eyre::eyre!(
                "coding_session timeout_secs must be 0 (default) or at least {MIN_DRIVER_TIMEOUT_SECS}"
            ));
        }
        let efforts = self.coding_session.engine.kind.efforts();
        if let Some(effort) = self
            .coding_session
            .effort
            .as_deref()
            .filter(|e| !efforts.contains(e))
        {
            return Err(eyre::eyre!(
                "coding_session effort must be one of {efforts:?}, got {effort:?}"
            ));
        }
        if let Some(remote) = &self.exec.remote {
            remote.validate()?;
        }
        if self.coding_session.executor.kind == ExecutorKind::Remote && self.exec.remote.is_none() {
            return Err(eyre::eyre!(
                "[coding_session.executor] kind = \"remote\" needs an [exec.remote] worker"
            ));
        }
        let chain = EngineEntry::parse_all(&self.coding_session.engine.fallback)
            .map_err(|e| eyre::eyre!("[coding_session.engine] {e}"))?;
        let chat_engine = self
            .claude_chat
            .effective_engine(&self.coding_session.engine);
        let chat_chain = EngineEntry::parse_all(&chat_engine.fallback)
            .map_err(|e| eyre::eyre!("[claude_chat.engine] {e}"))?;
        let codex_chat_engine = self
            .codex_chat
            .effective_engine_led_by(&self.coding_session.engine, CodingEngineKind::Codex);
        let codex_chat_chain = EngineEntry::parse_all(&codex_chat_engine.fallback)
            .map_err(|e| eyre::eyre!("[codex_chat.engine] {e}"))?;
        let names_bedrock = [
            (self.coding_session.engine.kind, chain),
            (chat_engine.kind, chat_chain),
            (codex_chat_engine.kind, codex_chat_chain),
        ]
        .iter()
        .any(|(kind, entries)| kind.is_bedrock() || entries.iter().any(|e| e.kind.is_bedrock()));
        self.coding_session.bedrock.validate(names_bedrock)?;
        self.coding_session.codex.validate()?;
        self.llm.logins.validate()?;
        // Validate agents have unique IDs
        let mut seen = std::collections::HashSet::new();
        for agent in &self.agents {
            if !seen.insert(&agent.id) {
                return Err(eyre::eyre!("Duplicate agent ID: {}", agent.id));
            }
            if let Some(logins) = agent.llm.as_ref().and_then(|llm| llm.logins.as_ref()) {
                logins
                    .validate()
                    .map_err(|e| eyre::eyre!("agent {}: {e}", agent.id))?;
            }
        }
        // Validate at most one default agent
        let default_count = self.agents.iter().filter(|a| a.default).count();
        if default_count > 1 {
            return Err(eyre::eyre!(
                "Multiple agents marked as default (only one allowed)"
            ));
        }
        // Validate teams have unique IDs
        seen.clear();
        for team in &self.teams {
            if !seen.insert(&team.id) {
                return Err(eyre::eyre!("Duplicate team ID: {}", team.id));
            }
        }
        // Validate timezone if set
        if let Some(ref tz) = self.timezone {
            tz.parse::<Tz>()
                .map_err(|_| eyre::eyre!("Invalid timezone '{}': use IANA names like 'Europe/London' or 'America/New_York'", tz))?;
        }
        Ok(())
    }

    /// Find the default agent config, or the first agent if none is marked default.
    pub fn default_agent(&self) -> Option<&AgentConfig> {
        self.agents
            .iter()
            .find(|a| a.default)
            .or_else(|| self.agents.first())
    }

    /// Look up an agent config by ID.
    pub fn agent_by_id(&self, id: &str) -> Option<&AgentConfig> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// Resolve the configured user timezone, falling back to UTC.
    pub fn user_timezone(&self) -> Tz {
        self.timezone
            .as_deref()
            .and_then(|s| s.parse::<Tz>().ok())
            .unwrap_or(chrono_tz::UTC)
    }

    /// Generate a default config TOML string.
    pub fn default_toml() -> String {
        r#"# Workforce Configuration
# See https://github.com/driftwerk/workforce for documentation.

[hub]
listen = "0.0.0.0:8090"
data_dir = "./data"
log_level = "info"
# policy_path = "policies/default.yaml"  # Auto-detected if present

[nats]
url = "nats://localhost:4222"

[memory]
backend = "sqlite"          # "sqlite" (default), "weaviate", "vellev", or "in_memory"
db_path = "data/memory.db"  # SQLite database path
# Embedding provider: "openai" for vector search, "none" for keyword-only (default).
# embedding_provider = "openai"
# embedding_model = "text-embedding-3-small"
# embedding_api_url = "https://api.openai.com/v1/embeddings"
# embedding_api_key = "sk-..."  # Or set OPENAI_API_KEY / WORKFORCE_EMBEDDING_API_KEY env var
# passive = false              # Post-turn memory extraction (default off)
# passive_model = "cheap"      # Model for passive extraction
# passive_min_turn_tokens = 200  # Skip trivial turns below this token count
# consolidation = false        # Periodic memory consolidation (default off)
# consolidation_interval_secs = 14400  # 4 hours
# consolidation_lookback_secs = 43200  # 12 hours — session scan window
# consolidation_model = "cheap"  # Model for the merge phase

[weaviate]
url = "http://localhost:8080"
# enabled = true  # Enable after running: docker compose -f deploy/docker-compose.weaviate.yml up -d
enabled = false

# Memories, the artifact index and code vectors are each kept where their
# own setting says, so one can move to vellev while the others stay:
#   [memory] backend            "vellev" keeps memories there
#   [artifacts] index_backend   "weaviate" (default, when [weaviate] enabled) or "vellev"
#   [knowledge] vector_backend  "weaviate" (default, when [weaviate] enabled) or "vellev"
#   [knowledge] vector_backfill_per_sec = 5    # pace of the hub's code-vector backfill; 0 = no limit
#   [knowledge] vector_reconcile_secs = 21600  # how often code vectors are compared with the code graph
#   [knowledge] vector_search_timeout_ms = 3000   # a code question waits this long for the vector store
#   [artifacts] index_search_timeout_ms = 3000    # an artifact search waits this long for the index
# [vellev]                    # Used when any of the three is "vellev"
# url = "http://localhost:18080"  # Or set WORKFORCE_VELLEV_URL
# api_key = "..."             # Required; or set WORKFORCE_VELLEV_API_KEY
# tenant = "acme"             # Required, created on the node by an operator; or set WORKFORCE_VELLEV_TENANT
# connect_timeout_secs = 300  # How long startup waits for the node
# The node embeds artifacts and code (and memories, without [embeddings])
# with its own bge-base-en-v1.5, so a node that holds any of those must run
# that model, whatever embeds the memories.

[llm]
# Provider options:
#   "anthropic"   — Standard API key, pay per token
#   "openrouter"  — OpenRouter proxy
#   "claude-code" — Your Claude plan, through the claude CLI
#   "codex"       — Your ChatGPT plan, through the codex CLI
#   "chatgpt"     — Your ChatGPT plan, called directly ([subscription] chatgpt_direct)
provider = "anthropic"
# api_key = "sk-ant-..."  # Or set env var (see below)
#
# For anthropic:  set ANTHROPIC_API_KEY env var
# For openrouter: set OPENROUTER_API_KEY env var
default_model = "claude-sonnet-4-20250514"
max_tokens = 8192
max_rounds = 50

# Failover chain: ordered list of models to try when the primary fails.
# failover = ["claude-3-5-haiku-20241022"]

# Model aliases: friendly names for models.
# [llm.aliases]
# fast = "claude-3-5-haiku-20241022"
# smart = "claude-opus-4-20250514"
# balanced = "claude-sonnet-4-20250514"

# Retry configuration for transient errors.
# [llm.retry]
# max_retries = 3
# initial_backoff_ms = 1000
# backoff_multiplier = 2.0
# max_backoff_ms = 30000
# retry_overloaded = false   # Retry 529 Overloaded before failover (keeps Bedrock load off metered fallback)

# Model health tracking.
# [llm.health]
# failure_threshold = 3   # Failures within window to mark degraded
# window_secs = 300       # Time window for counting failures
# recovery_secs = 120     # Seconds before retrying a degraded model

# Mandatory LLM spend governance. Values live only in config files (the
# runtime overlay rejects this table); 0 disables an individual ceiling.
# Per-agent tightening: [budget.agents.<id>] with agent_daily_usd /
# job_total_usd / job_max_runs / job_expiry_days (min-merged, tighten-only).
[budget]
enabled = true
hub_daily_usd = 1000.0
agent_daily_usd = 200.0
identity_daily_usd = 50.0
job_total_usd = 25.0
job_max_runs = 100
job_expiry_days = 7
min_recurring_interval_secs = 300
confirm_recurring_daily_usd = 5.0

# User timezone (IANA name). Affects system prompt, quiet hours, and scheduling.
# timezone = "Europe/London"

# ─── GitHub Integration ───
# Requires a GitHub Personal Access Token (PAT).
# [github]
# token = "ghp_..."            # Or set GITHUB_TOKEN env var
# default_owner = "your-org"   # Default owner for tool calls
# base_url = "https://api.github.com"  # GitHub Enterprise override

# ─── Agents ───
# Each [[agents]] entry gets its own workspace with identity files (SOUL.md, etc.).
# Add more [[agents]] entries to run multiple agents.

[[agents]]
id = "default"
name = "Agent"
workspace = "./data/agents/default"
default = true

# [[agents]]
# id = "architect"
# name = "Architect"
# model = "claude-sonnet-4-20250514"
# workspace = "./data/agents/architect"
# capabilities = ["design", "code-review", "planning"]

# Heartbeat — periodic agent check-ins.
# [heartbeat]
# enabled = true
# interval_secs = 1800        # 30 minutes
# active_start = "08:00"      # In user timezone
# active_end = "23:00"
# channels = ["slack"]         # Deliver to these channels (empty = internal only)
# deliver = "actionable_only"  # "all" or "actionable_only" (suppresses HEARTBEAT_OK)
# history_size = 20            # Keep last N heartbeat results
# prompt = "..."               # Custom heartbeat prompt (optional)

# Example team definition:
# [[teams]]
# id = "backend"
# name = "Backend Dev Team"
# lead = "architect"
# pattern = "hierarchical"
# require_review = true
#
# [[teams.members]]
# id = "architect"
# role = "lead"
# model = "anthropic/claude-sonnet-4"
# capabilities = ["design", "code-review"]
#
# [[teams.members]]
# id = "developer"
# role = "backend-dev"
# capabilities = ["code-write", "test"]
"#
        .to_string()
    }
}

fn default_max_daily_notes_chars() -> usize {
    8000
}

fn default_listen() -> String {
    "0.0.0.0:8090".to_string()
}
fn default_data_dir() -> String {
    "./data".to_string()
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_nats_url() -> String {
    "nats://localhost:4222".to_string()
}
fn default_weaviate_url() -> String {
    "http://localhost:8080".to_string()
}
fn default_provider() -> String {
    "openrouter".to_string()
}
fn default_model() -> String {
    "anthropic/claude-sonnet-4".to_string()
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_max_rounds() -> u32 {
    50
}
fn default_interactive_max_rounds() -> u32 {
    16
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curator_defaults_apply_without_a_config_file() {
        // Without a workforce.toml the curator uses `CuratorConfig::default()`;
        // it must match what an empty `[curator]` section parses to.
        let parsed: CuratorConfig = toml::from_str("").unwrap();
        let default = CuratorConfig::default();
        assert_eq!(default.max_tokens, parsed.max_tokens);
        assert_eq!(default.notes_max_tokens, parsed.notes_max_tokens);
        assert_eq!(default.max_turns, parsed.max_turns);
        assert_eq!(default.context_safety_tokens, parsed.context_safety_tokens);
        assert!(default.max_tokens > 0 && default.notes_max_tokens > 0 && default.max_turns > 0);
    }

    #[test]
    fn trace_retention_defaults_to_a_bounded_window() {
        let default: TracingConfig = toml::from_str("").expect("empty section parses");
        assert_eq!(default.retention_days, 14);
        assert!(default.enabled);

        // The case that matters for existing deployments: a workforce.toml
        // with no [tracing] section at all still gets a bounded window.
        let absent: WorkforceConfig =
            toml::from_str("[hub]\ndata_dir = \"./data\"\n").expect("config without [tracing]");
        assert_eq!(absent.tracing.retention_days, 14);

        let explicit: TracingConfig =
            toml::from_str("retention_days = 0").expect("explicit zero parses");
        assert_eq!(
            explicit.retention_days, 0,
            "zero is an opt-out, not a default"
        );
    }

    #[test]
    fn telemetry_is_kept_forever_and_resolved_approvals_a_month_by_default() {
        let absent: WorkforceConfig =
            toml::from_str("[hub]\ndata_dir = \"./data\"\n").expect("config without either");
        assert_eq!(
            (
                absent.telemetry.activity_retention_days,
                absent.telemetry.cost_retention_days,
                absent.approvals.retention_days,
            ),
            (0, 0, 30)
        );
        let set: WorkforceConfig = toml::from_str(
            "[telemetry]\nactivity_retention_days = 400\ncost_retention_days = 730\n\
             [approvals]\nretention_days = 7\n",
        )
        .unwrap();
        assert_eq!(set.telemetry.backend, "sqlite");
        assert_eq!(
            (
                set.telemetry.activity_retention_days,
                set.telemetry.cost_retention_days,
                set.approvals.retention_days,
            ),
            (400, 730, 7)
        );
    }

    #[test]
    fn coding_session_budget_defaults_on_zero_and_honours_overrides() {
        let unset = CodingSessionConfig::default();
        assert_eq!(unset.effective_max_turns(), DEFAULT_DRIVER_MAX_TURNS);
        assert_eq!(
            unset.effective_timeout(),
            Duration::from_secs(DEFAULT_DRIVER_TIMEOUT_SECS)
        );

        let configured = CodingSessionConfig {
            max_turns: 40,
            timeout_secs: 900,
            ..Default::default()
        };
        assert_eq!(configured.effective_max_turns(), 40);
        assert_eq!(configured.effective_timeout(), Duration::from_secs(900));
    }

    #[test]
    fn coding_session_round_defaults_parse_and_default_empty() {
        let unset = CodingSessionConfig::default();
        assert!(unset.preamble.is_empty());
        assert!(unset.cache_dir_path().is_none());
        assert!(unset.model.is_none() && unset.effort.is_none());

        let config: WorkforceConfig = toml::from_str(
            r#"
[coding_session]
preamble = "CI is the verifier."
cache_dir = "/fast/cache"
model = "sonnet"
effort = "high"
"#,
        )
        .unwrap();
        let coding = &config.coding_session;
        assert_eq!(coding.preamble, "CI is the verifier.");
        assert_eq!(
            coding.cache_dir_path(),
            Some(std::path::Path::new("/fast/cache"))
        );
        assert_eq!(coding.model.as_deref(), Some("sonnet"));
        assert_eq!(coding.effort.as_deref(), Some("high"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_an_unknown_coding_session_effort() {
        let mut config = WorkforceConfig::default();
        config.coding_session.effort = Some("extreme".into());
        let err = config
            .validate()
            .expect_err("unknown effort must be rejected");
        assert!(err.to_string().contains("effort"), "{err}");

        for effort in CLAUDE_CODE_EFFORTS {
            config.coding_session.effort = Some((*effort).into());
            assert!(config.validate().is_ok(), "{effort} is a valid effort");
        }
    }

    #[test]
    fn removed_oauth_system_prefix_parses_harmlessly() {
        let config: WorkforceConfig = toml::from_str(
            r#"
[llm]
provider = "anthropic"
oauth_system_prefix = "You are Claude Code, Anthropic's official CLI for Claude."

[[agents]]
id = "allan"
name = "Allan"
[agents.llm]
oauth_system_prefix = ""
"#,
        )
        .expect("a removed field still parses");
        assert!(config.validate().is_ok());
        let workspace =
            AgentWorkspaceConfig::parse("[llm]\noauth_system_prefix = \"prefix\"\n").unwrap();
        for written in [
            serde_json::to_string(&config.llm).unwrap(),
            serde_json::to_string(&config.agents).unwrap(),
            serde_json::to_string(&workspace).unwrap(),
        ] {
            assert!(!written.contains("oauth_system_prefix"), "{written}");
        }
    }

    #[test]
    fn claude_max_global_config_fails_validate_with_pointer() {
        let mut config = WorkforceConfig::default();
        for removed in ["claude-max", "Max", "oauth"] {
            config.llm.provider = removed.into();
            let err = config.validate().unwrap_err().to_string();
            assert!(
                err.contains(removed) && err.contains(REMOVED_PROVIDER_POINTER),
                "{err}"
            );
        }
        config.llm.provider = "anthropic".into();
        config.llm.api_key = Some(concat!("sk-ant", "-oat01-abcdefghijklmnop").into());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains(REMOVED_PROVIDER_POINTER), "{err}");
        config.llm.api_key = Some(concat!("sk-ant", "-api03-abcdefghijklmnop").into());
        config.validate().expect("an API key validates");
    }

    fn remote_runner() -> RemoteRunnerConfig {
        RemoteRunnerConfig {
            addr: "worker.prod.harbor.local:7444".into(),
            server_name: "worker.prod.harbor.local".into(),
            ca_pem: "-----BEGIN CERTIFICATE-----\nca\n-----END CERTIFICATE-----".into(),
            cert_pem: "-----BEGIN CERTIFICATE-----\nhub\n-----END CERTIFICATE-----".into(),
            key_pem: "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----".into(),
        }
    }

    #[test]
    fn validate_accepts_a_complete_remote_runner() {
        let mut config = WorkforceConfig::default();
        config.exec.remote = Some(remote_runner());
        assert!(config.validate().is_ok());
        assert!(
            !format!("{:?}", config.exec.remote).contains("PRIVATE KEY"),
            "the key must not reach logs through Debug"
        );
    }

    #[test]
    fn codex_kinds_parse_and_a_codex_chain_is_subscription_only() {
        let entry = EngineEntry::parse("codex:sol").unwrap();
        assert_eq!(entry.kind, CodingEngineKind::Codex);
        assert_eq!(entry.model.as_deref(), Some("sol"));
        let metered = EngineEntry::parse("codex_api:gpt-6-astra").unwrap();
        assert_eq!(metered.kind, CodingEngineKind::CodexApi);
        assert!(
            CodingEngineKind::Codex.is_subscription()
                && !CodingEngineKind::CodexApi.is_subscription()
        );
        assert_eq!(CodingEngineKind::Codex.efforts(), CODEX_EFFORTS);
        assert_eq!(CodingEngineKind::ClaudeCode.efforts(), CLAUDE_CODE_EFFORTS);

        let chain: CodingEngineConfig =
            toml::from_str("kind = \"codex\"\nfallback = [\"claude_code:opus\"]\n").unwrap();
        assert_eq!(
            chain.kinds(),
            vec![CodingEngineKind::Codex, CodingEngineKind::ClaudeCode]
        );
        assert!(chain.subscription_only());
        let metered_chain: CodingEngineConfig =
            toml::from_str("kind = \"codex\"\nfallback = [\"codex_api:sol\"]\n").unwrap();
        assert!(!metered_chain.subscription_only());
    }

    #[test]
    fn codex_config_has_defaults_and_refuses_malformed_aliases() {
        let config = CodingCodexConfig::default();
        assert_eq!(
            config.effective_tool_timeout().as_secs(),
            DEFAULT_CODEX_TOOL_TIMEOUT_SECS
        );
        assert_eq!(config.resolve_model("astra"), "gpt-6-astra");
        assert_eq!(
            config.resolve_model("gpt-5.5"),
            "gpt-5.5",
            "an unknown name is a model id"
        );
        let custom: CodingCodexConfig = toml::from_str(
            "tool_timeout_secs = 30\n[models]\nsol = \"gpt-5.7-sol\"\nfast = \"gpt-5.6-luna\"\n",
        )
        .unwrap();
        assert_eq!(custom.effective_tool_timeout().as_secs(), 30);
        assert_eq!(
            custom.resolve_model("sol"),
            "gpt-5.7-sol",
            "an override wins"
        );
        assert_eq!(custom.resolve_model("fast"), "gpt-5.6-luna");
        assert_eq!(custom.model_aliases().len(), 5);
        assert!(custom.validate().is_ok());
        let bad: CodingCodexConfig = toml::from_str("[models]\n\"two words\" = \"x\"\n").unwrap();
        assert!(bad
            .validate()
            .unwrap_err()
            .to_string()
            .contains("whitespace"));

        let mut wf = WorkforceConfig::default();
        wf.coding_session.engine.kind = CodingEngineKind::Codex;
        wf.coding_session.effort = Some("xhigh".into());
        assert!(wf.validate().is_ok(), "xhigh is a Codex effort");
        wf.coding_session.engine.kind = CodingEngineKind::ClaudeCode;
        assert!(wf.validate().unwrap_err().to_string().contains("xhigh"));
    }

    #[test]
    fn codex_chat_follows_the_coding_session_led_by_codex() {
        let coding = CodingEngineConfig {
            fallback: vec!["claude_code:opus".into()],
            ..Default::default()
        };
        let inherit: ClaudeChatConfig = toml::from_str("").unwrap();
        let led = inherit.effective_engine_led_by(&coding, CodingEngineKind::Codex);
        assert_eq!(led.kind, CodingEngineKind::Codex);
        assert_eq!(led.fallback, coding.fallback);
        let own: ClaudeChatConfig = toml::from_str("[engine]\nkind = \"codex_api\"\n").unwrap();
        assert_eq!(
            own.effective_engine_led_by(&coding, CodingEngineKind::Codex)
                .kind,
            CodingEngineKind::CodexApi
        );
        let wf: WorkforceConfig =
            toml::from_str("[codex_chat.engine]\nkind = \"codex\"\nfallback = [\"codex:\"]\n")
                .unwrap();
        assert!(wf
            .validate()
            .unwrap_err()
            .to_string()
            .contains("[codex_chat.engine]"));
    }

    #[test]
    fn chat_executor_and_engine_follow_the_coding_session_unless_set() {
        let coding_exec = ExecutorConfig {
            kind: ExecutorKind::Remote,
            scope_ttl_secs: 99,
        };
        let inherit: ClaudeChatConfig = toml::from_str("").unwrap();
        assert_eq!(inherit.effective_executor(&coding_exec), coding_exec);
        let local: ClaudeChatConfig = toml::from_str("executor = \"local\"\n").unwrap();
        let effective = local.effective_executor(&coding_exec);
        assert_eq!(effective.kind, ExecutorKind::Local);
        assert_eq!(effective.scope_ttl_secs, 99);

        let coding_engine = CodingEngineConfig {
            fallback: vec!["claude_code:opus".into()],
            ..Default::default()
        };
        assert_eq!(inherit.effective_engine(&coding_engine), coding_engine);
        let own: ClaudeChatConfig =
            toml::from_str("[engine]\nfallback = [\"claude_code_bedrock:standard\"]\n").unwrap();
        assert_eq!(
            own.effective_engine(&coding_engine).fallback,
            vec!["claude_code_bedrock:standard".to_string()]
        );

        let mut config = WorkforceConfig::default();
        config.claude_chat.engine = Some(own.engine.clone().unwrap());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("assume_role_arn is unset"), "{err}");
        config.claude_chat.engine = Some(CodingEngineConfig {
            fallback: vec!["model:".into()],
            ..Default::default()
        });
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("[claude_chat.engine]"), "{err}");
    }

    #[test]
    fn bedrock_engine_entries_need_the_bedrock_section() {
        let entry = EngineEntry::parse("claude_code_bedrock:standard").unwrap();
        assert_eq!(entry.kind, CodingEngineKind::ClaudeCodeBedrock);
        assert_eq!(entry.model.as_deref(), Some("standard"));

        let mut config = WorkforceConfig::default();
        config.coding_session.engine.fallback = vec!["claude_code_bedrock:standard".into()];
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("assume_role_arn is unset"), "{err}");

        config.coding_session.bedrock.assume_role_arn = Some("not-an-arn".into());
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("not an IAM role ARN"), "{err}");

        config.coding_session.bedrock.assume_role_arn =
            Some("arn:aws:iam::123456789012:role/bedrock-invoke".into());
        assert!(config.validate().is_ok());
        config.coding_session.bedrock.max_session_secs = 60;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_session_secs"), "{err}");

        let parsed: CodingSessionConfig = toml::from_str(
            "[bedrock]\nassume_role_arn = \"arn:aws:iam::1:role/r\"\nregion = \"eu-west-2\"\n",
        )
        .unwrap();
        assert_eq!(parsed.bedrock.region.as_deref(), Some("eu-west-2"));
        assert_eq!(
            parsed.bedrock.effective_max_session().as_secs(),
            DEFAULT_BEDROCK_MAX_SESSION_SECS
        );
        let absent: CodingSessionConfig = toml::from_str("").unwrap();
        assert!(!absent.bedrock.is_configured());
    }

    #[test]
    fn engine_chain_entries_parse_and_bad_ones_name_themselves() {
        let entry = EngineEntry::parse("claude_code:opus").unwrap();
        assert_eq!(entry.kind, CodingEngineKind::ClaudeCode);
        assert_eq!(entry.model.as_deref(), Some("opus"));
        assert_eq!(EngineEntry::parse(" claude_code ").unwrap().model, None);
        for bad in ["claude_code:", "model:", ""] {
            let err = EngineEntry::parse(bad).unwrap_err().to_string();
            assert!(err.contains(bad.trim()), "{bad}: {err}");
        }
        for spec in [
            "openrouter:anthropic:cheap",
            "bedrock:anthropic:standard",
            "gpt-5",
        ] {
            let entry = EngineEntry::parse(spec).unwrap();
            assert_eq!(entry.kind, CodingEngineKind::Model, "{spec}");
            assert_eq!(entry.model.as_deref(), Some(spec));
        }
        let explicit = EngineEntry::parse("model:openrouter:anthropic:cheap").unwrap();
        assert_eq!(
            (explicit.kind, explicit.model.as_deref()),
            (CodingEngineKind::Model, Some("openrouter:anthropic:cheap"))
        );

        let mut engine = CodingEngineConfig::default();
        assert!(engine.subscription_only());
        engine.fallback = vec!["claude_code:opus".into()];
        assert!(engine.subscription_only());
        engine.fallback.push("openrouter:anthropic:cheap".into());
        assert!(!engine.subscription_only());
        engine.kind = CodingEngineKind::Model;
        engine.fallback.clear();
        assert!(!engine.subscription_only());

        let mut config = WorkforceConfig::default();
        config.coding_session.engine.fallback = vec!["claude_code:opus".into(), "model:".into()];
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("[coding_session.engine]") && err.contains("model:"),
            "{err}"
        );

        let absent: CodingSessionConfig = toml::from_str("").unwrap();
        assert_eq!(absent.engine, CodingEngineConfig::default());
        assert_eq!(
            absent.engine.fallback_on.len(),
            3,
            "every trigger advances by default"
        );
        let parsed: CodingSessionConfig = toml::from_str(
            "[engine]\nkind = \"claude_code\"\nfallback = [\"claude_code:opus\"]\nfallback_on = [\"rate_limited\"]\n",
        )
        .unwrap();
        assert_eq!(
            parsed.engine.fallback_on,
            vec![FallbackTrigger::RateLimited]
        );
        let err = toml::from_str::<CodingSessionConfig>("[engine]\nfallback_on = [\"weather\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("weather"), "{err}");
    }

    #[test]
    fn validate_accepts_a_remote_worker_beside_the_local_runner_socket() {
        let mut config = WorkforceConfig::default();
        config.exec.remote = Some(remote_runner());
        config.exec.runner_socket = Some(PathBuf::from("/run/workforce/exec.sock"));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_remote_executor_needs_a_remote_worker() {
        let mut config = WorkforceConfig::default();
        config.coding_session.executor.kind = ExecutorKind::Remote;
        let err = config.validate().expect_err("no worker to run on");
        assert!(err.to_string().contains("[exec.remote]"), "{err}");
        config.exec.remote = Some(remote_runner());
        assert!(config.validate().is_ok());
        assert_eq!(config.coding_session.executor.scope_ttl_secs, 2 * 60 * 60);
    }

    #[test]
    fn validate_names_a_missing_or_malformed_remote_runner_field() {
        let mut config = WorkforceConfig::default();
        config.exec.remote = Some(RemoteRunnerConfig {
            server_name: String::new(),
            ..remote_runner()
        });
        let err = config
            .validate()
            .expect_err("an empty field must be rejected");
        assert!(err.to_string().contains("server_name"), "{err}");

        config.exec.remote = Some(RemoteRunnerConfig {
            key_pem: "not a pem".into(),
            ..remote_runner()
        });
        let err = config
            .validate()
            .expect_err("a non-PEM key must be rejected");
        assert!(err.to_string().contains("key_pem"), "{err}");
    }

    #[test]
    fn validate_rejects_a_coding_session_timeout_below_the_floor() {
        let mut config = WorkforceConfig::default();
        config.coding_session.timeout_secs = MIN_DRIVER_TIMEOUT_SECS - 1;
        let err = config
            .validate()
            .expect_err("below the floor must be rejected");
        assert!(
            err.to_string().contains("timeout_secs"),
            "error should name the offending key: {err}"
        );

        config.coding_session.timeout_secs = MIN_DRIVER_TIMEOUT_SECS;
        assert!(config.validate().is_ok(), "the floor itself is allowed");

        config.coding_session.timeout_secs = 0;
        assert!(config.validate().is_ok(), "0 means default, not invalid");
    }

    /// A relative `data_dir` must resolve against the config file's directory,
    /// not the process CWD — the coding.session/mcp-serve failure where
    /// `./data/vault.enc` resolved into a code worktree and opened an empty
    /// vault. Loading the same config from a different CWD must yield the same
    /// absolute `data_dir`.
    #[test]
    fn relative_data_dir_anchors_to_config_dir_not_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("workforce.toml");
        std::fs::write(
            &cfg_path,
            "[hub]\ndata_dir = \"./data\"\n\
             [vault]\npath = \"./data/vault.enc\"\nkey_file = \"./data/vault-key.txt\"\n",
        )
        .expect("write config");

        let config = WorkforceConfig::load(&cfg_path).expect("load");
        let expected = dir.path().join("data");
        assert_eq!(
            Path::new(&config.hub.data_dir),
            expected,
            "data_dir anchors to the config dir"
        );
        assert_eq!(
            config.vault.path.as_deref().map(Path::new),
            Some(dir.path().join("data/vault.enc").as_path()),
            "vault path anchors too — this is the file mcp-serve opens"
        );
        assert_eq!(
            config.vault.key_file.as_deref().map(Path::new),
            Some(dir.path().join("data/vault-key.txt").as_path()),
            "vault key_file anchors too"
        );
        // Absolute path is the resolved config dir regardless of the caller CWD.
        assert!(Path::new(&config.hub.data_dir).is_absolute());
    }

    /// An absolute `data_dir` is left untouched.
    #[test]
    fn absolute_data_dir_is_not_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("workforce.toml");
        std::fs::write(&cfg_path, "[hub]\ndata_dir = \"/srv/workforce/data\"\n")
            .expect("write config");
        let config = WorkforceConfig::load(&cfg_path).expect("load");
        assert_eq!(config.hub.data_dir, "/srv/workforce/data");
    }

    #[test]
    fn retry_watchdog_timeouts_parse_and_default() {
        // Absent block → defaults.
        let absent: WorkforceConfig = toml::from_str("").unwrap();
        assert!(absent.llm.retry.is_none());
        let defaults = RetryConfig::default();
        assert_eq!(defaults.first_token_timeout_secs, 45);
        assert_eq!(defaults.stall_timeout_secs, 180);
        assert_eq!(defaults.request_timeout_secs, 300);
        assert!(defaults.model_timeouts.is_empty());

        // Explicit values + a per-model override parse.
        let cfg: WorkforceConfig = toml::from_str(
            r#"
[llm.retry]
first_token_timeout_secs = 30
stall_timeout_secs = 120

[llm.retry.model_timeouts."gemini-3.1"]
first_token_timeout_secs = 10
"#,
        )
        .unwrap();
        let retry = cfg.llm.retry.expect("retry block parses");
        assert_eq!(retry.first_token_timeout_secs, 30);
        assert_eq!(retry.stall_timeout_secs, 120);
        // Omitted field keeps its default.
        assert_eq!(retry.request_timeout_secs, 300);
        let over = &retry.model_timeouts["gemini-3.1"];
        assert_eq!(over.first_token_timeout_secs, Some(10));
        assert_eq!(over.stall_timeout_secs, None);
    }

    #[test]
    fn code_context_defaults_off_and_is_flag_toggleable() {
        // Default (constructed) is OFF — rolled out per-install once validated.
        assert!(!CodeContextConfig::default().enabled);

        // A whole config with NO `[code_context]` section resolves to OFF.
        let absent: WorkforceConfig = toml::from_str("").unwrap();
        assert!(!absent.code_context.enabled, "section absent → OFF");

        // The section present but `enabled` omitted still resolves to OFF.
        let bare: WorkforceConfig = toml::from_str("[code_context]\n").unwrap();
        assert!(!bare.code_context.enabled, "bare section → OFF");

        // It is a real flag: an explicit `true` turns injection on.
        let on: WorkforceConfig = toml::from_str("[code_context]\nenabled = true\n").unwrap();
        assert!(on.code_context.enabled, "explicit true → ON");
    }

    #[test]
    fn default_chat_exposed_tools_excludes_admin_and_messaging_surface() {
        // Hub-internal admin tools must not be reachable from a user-driven
        // /cc thread by default. If you legitimately need one of these in
        // chat, override `[claude_chat].exposed_tools` explicitly — don't
        // weaken the default.
        for forbidden in [
            "vault.*",
            "system.*",
            "workflow.*",
            "slack.*",
            "engagement.*",
        ] {
            assert!(
                !DEFAULT_CHAT_EXPOSED_TOOLS.contains(&forbidden),
                "{forbidden} must not appear in DEFAULT_CHAT_EXPOSED_TOOLS"
            );
        }
        // The user-visible adapters that motivated this change must be in.
        for required in ["linear.*", "jira.*", "metabase.*", "google.*", "github.*"] {
            assert!(
                DEFAULT_CHAT_EXPOSED_TOOLS.contains(&required),
                "{required} must appear in DEFAULT_CHAT_EXPOSED_TOOLS"
            );
        }
        // Bare action-style tool names — exact entries, not `.*` patterns
        // (which a bare name never matches).
        for bare in ["memory", "notes"] {
            assert!(
                DEFAULT_CHAT_EXPOSED_TOOLS.contains(&bare),
                "{bare} must appear in DEFAULT_CHAT_EXPOSED_TOOLS as an exact name"
            );
        }
    }

    #[test]
    fn memory_config_lesson_defaults_are_off_with_sane_floors() {
        let cfg = MemoryConfig::default();
        assert!(!cfg.lesson_extract);
        assert!(!cfg.lesson_inject);
        assert_eq!(cfg.lesson_inject_limit, 3);
        assert!((cfg.lesson_inject_min_confidence - 0.4).abs() < f32::EPSILON);
        assert!((cfg.lesson_inject_min_importance - 0.4).abs() < f32::EPSILON);
        assert!(cfg.lesson_extract_model.is_none());
        assert!(cfg.lesson_extract_prompt.is_none());
    }

    #[test]
    fn memory_config_round_trips_lesson_fields_through_toml() {
        let toml_input = r#"
            backend = "in-memory"
            db_path = "x"
            embedding_provider = "none"
            embedding_model = ""
            embedding_api_url = ""
            lesson_extract = true
            lesson_inject = true
            lesson_inject_limit = 5
            lesson_inject_min_confidence = 0.5
            lesson_inject_min_importance = 0.55
        "#;
        let cfg: MemoryConfig = toml::from_str(toml_input).expect("memory toml parses");
        assert!(cfg.lesson_extract);
        assert!(cfg.lesson_inject);
        assert_eq!(cfg.lesson_inject_limit, 5);
        assert!((cfg.lesson_inject_min_confidence - 0.5).abs() < f32::EPSILON);
        assert!((cfg.lesson_inject_min_importance - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn claude_chat_config_inherits_mcp_enabled_when_unset() {
        let chat = ClaudeChatConfig::default();
        let mcp_off = McpConfig::default();
        let mcp_on = McpConfig {
            enabled: true,
            ..McpConfig::default()
        };
        assert!(!chat.effective_mcp_enabled(&mcp_off));
        assert!(chat.effective_mcp_enabled(&mcp_on));

        let chat_explicit_off = ClaudeChatConfig {
            mcp_enabled: Some(false),
            ..ClaudeChatConfig::default()
        };
        // Explicit off wins over [mcp].enabled=true.
        assert!(!chat_explicit_off.effective_mcp_enabled(&mcp_on));
    }

    #[test]
    fn mcp_pat_server_config_round_trips_through_toml() {
        let toml = r#"
            [mcp]
            schema_version = 1
            [mcp.servers.github]
            endpoint = "https://api.githubcopilot.com/mcp/"
            allowed_hosts = ["api.githubcopilot.com"]
            shadows = []

            [mcp.servers.github.auth]
            kind = "pat"
            vault_path = "skills/github/token"
        "#;
        #[derive(Deserialize)]
        struct Wrapper {
            mcp: McpConfig,
        }
        let parsed: Wrapper = toml::from_str(toml).unwrap();
        let server = parsed.mcp.servers.get("github").expect("github server");
        assert_eq!(server.endpoint, "https://api.githubcopilot.com/mcp/");
        assert_eq!(server.transport, "streamable-http");
        assert_eq!(server.refresh_interval_secs, 3600);
        match &server.auth {
            McpAuthConfig::Pat { vault_path } => {
                assert_eq!(vault_path, "skills/github/token");
            }
            other => panic!("expected Pat, got {other:?}"),
        }
    }

    #[test]
    fn mcp_oauth_server_config_round_trips_through_toml() {
        let toml = r#"
            [mcp.servers.linear]
            endpoint = "https://mcp.linear.app/mcp"
            allowed_hosts = ["mcp.linear.app"]
            shadows = ["linear_*"]

            [mcp.servers.linear.auth]
            kind = "oauth2"
            provider = "linear"
            scopes = ["read", "write"]
        "#;
        #[derive(Deserialize)]
        struct Wrapper {
            mcp: McpConfig,
        }
        let parsed: Wrapper = toml::from_str(toml).unwrap();
        let server = parsed.mcp.servers.get("linear").expect("linear server");
        assert_eq!(server.shadows, vec!["linear_*".to_string()]);
        match &server.auth {
            McpAuthConfig::Oauth2 {
                binding,
                provider,
                scopes,
                ..
            } => {
                assert_eq!(*binding, McpAuthBinding::User);
                assert_eq!(provider, "linear");
                assert_eq!(scopes, &vec!["read".to_string(), "write".to_string()]);
            }
            other => panic!("expected Oauth2, got {other:?}"),
        }
    }

    #[test]
    fn claude_chat_config_uses_default_when_exposed_tools_empty() {
        let chat = ClaudeChatConfig::default();
        let resolved = chat.effective_exposed_tools();
        assert_eq!(resolved.len(), DEFAULT_CHAT_EXPOSED_TOOLS.len());
        for pat in DEFAULT_CHAT_EXPOSED_TOOLS {
            assert!(resolved.iter().any(|s| s == pat));
        }

        let custom = ClaudeChatConfig {
            exposed_tools: vec!["linear.*".into()],
            ..ClaudeChatConfig::default()
        };
        assert_eq!(custom.effective_exposed_tools(), vec!["linear.*"]);
    }

    #[test]
    fn claude_chat_config_round_trips_model_effort_and_rotation() {
        let toml = r#"
[claude_chat]
model = "opus"
effort = "high"
rotate_after_turns = 80
rotate_after_idle_mins = 480
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.claude_chat.model.as_deref(), Some("opus"));
        assert_eq!(config.claude_chat.effort.as_deref(), Some("high"));
        assert_eq!(config.claude_chat.rotate_after_turns, Some(80));
        assert_eq!(config.claude_chat.rotate_after_idle_mins, Some(480));

        let defaults = WorkforceConfig::from_toml("").unwrap();
        assert!(defaults.claude_chat.model.is_none());
        assert!(defaults.claude_chat.effort.is_none());
        assert!(defaults.claude_chat.rotate_after_turns.is_none());
        assert!(defaults.claude_chat.rotate_after_idle_mins.is_none());
    }

    #[test]
    fn metabase_sync_defaults_and_corrects_nonsense() {
        let defaults = WorkforceConfig::from_toml("").unwrap().skills.metabase;
        assert!(defaults.schema_sync, "on by default");
        assert_eq!(defaults.interval(), Duration::from_secs(86_400));
        assert_eq!(defaults.caps(), (25, 1_000));
        assert_eq!(MetabaseSyncConfig::default(), defaults);

        let off: MetabaseSyncConfig = toml::from_str("schema_sync = false").unwrap();
        assert!(!off.schema_sync);
        assert_eq!(
            off.caps(),
            defaults.caps(),
            "disabling the pass does not disturb the other bounds"
        );

        let zeroed: MetabaseSyncConfig = toml::from_str(
            "sync_interval_secs = 0\ntables_with_columns = 0\nmax_requests_per_pass = 0",
        )
        .unwrap();
        assert!(
            zeroed.interval() >= Duration::from_secs(60),
            "no busy-wait loop"
        );
        let (tables, requests) = zeroed.caps();
        assert!(tables >= 1);
        assert!(
            requests > tables,
            "the budget must leave room for the metadata call as well as the tables"
        );
    }

    #[test]
    fn claim_grounding_defaults_off() {
        let defaults = WorkforceConfig::from_toml("").unwrap().claim_grounding;
        assert!(!defaults.enabled);
        assert!(
            !defaults.flag_failed_tools,
            "failed-tool flag is default-off"
        );
        assert!(!defaults.data_claims, "data-claim grounding is default-off");
        assert!(!defaults.commitments, "commitment check is default-off");
        assert!(
            defaults.model.is_none(),
            "grounding model defaults to agent default"
        );
        assert_eq!(ClaimGroundingConfig::default(), defaults);
        assert_eq!(
            defaults.review_budget(),
            (
                Duration::from_secs(DEFAULT_REVIEW_TIMEOUT_SECS),
                Duration::from_secs(DEFAULT_REVIEW_SYNTHESIS_RESERVE_SECS)
            ),
            "an unconfigured install gets the documented default budget"
        );

        // Stale keys from configs written before the editor/conclusion-review
        // removal (2026-08-01) must parse harmlessly — no deny_unknown_fields.
        let explicit: ClaimGroundingConfig =
            toml::from_str("enabled = true\nreview_conclusion = true\nrevise = true").unwrap();
        assert!(explicit.enabled);
        assert!(
            !explicit.flag_failed_tools,
            "omitted flag_failed_tools stays off"
        );
        assert!(!explicit.data_claims, "omitted data_claims stays off");
        assert!(!explicit.commitments, "omitted commitments stays off");
        assert!(explicit.model.is_none(), "omitted model stays None");

        let off: ClaimGroundingConfig =
            toml::from_str("enabled = false\nflag_failed_tools = false").unwrap();
        assert!(!off.flag_failed_tools);

        // The budget is a pair, and only the pair is meaningful: a reserve of
        // zero discards every write-up, and a reserve that swallows the budget
        // leaves nothing to gather. Both are corrected, never obeyed.
        let tuned: ClaimGroundingConfig = toml::from_str(
            "enabled = true\nreview_timeout_secs = 120\nreview_synthesis_reserve_secs = 30",
        )
        .unwrap();
        assert_eq!(
            tuned.review_budget(),
            (Duration::from_secs(120), Duration::from_secs(30))
        );

        let zero_reserve: ClaimGroundingConfig = toml::from_str(
            "enabled = true\nreview_timeout_secs = 60\nreview_synthesis_reserve_secs = 0",
        )
        .unwrap();
        let (total, reserve) = zero_reserve.review_budget();
        assert_eq!(total, Duration::from_secs(60));
        assert!(reserve > Duration::ZERO, "a zero reserve is corrected");

        let greedy_reserve: ClaimGroundingConfig = toml::from_str(
            "enabled = true\nreview_timeout_secs = 30\nreview_synthesis_reserve_secs = 90",
        )
        .unwrap();
        let (total, reserve) = greedy_reserve.review_budget();
        assert!(
            reserve < total,
            "a reserve at or above the budget leaves no research time"
        );

        let on: ClaimGroundingConfig =
            toml::from_str("enabled = false\ndata_claims = true\ncommitments = true").unwrap();
        assert!(on.data_claims);
        assert!(on.commitments);
        let round: ClaimGroundingConfig = toml::from_str(&toml::to_string(&on).unwrap()).unwrap();
        assert_eq!(round, on, "serde round-trip preserves the new flags");

        let with_model: ClaimGroundingConfig =
            toml::from_str("enabled = true\nmodel = \"cheap\"").unwrap();
        assert_eq!(with_model.model.as_deref(), Some("cheap"));
    }

    #[test]
    fn interactive_max_rounds_defaults_and_overrides() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.llm.interactive_max_rounds, 16);
        assert_eq!(config.llm.max_rounds, 50, "automated default untouched");

        let toml = r#"
[llm]
interactive_max_rounds = 24

[[agents]]
id = "a"
name = "A"
llm.interactive_max_rounds = 8

[[agents]]
id = "b"
name = "B"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.llm.interactive_max_rounds, 24);
        let a = config.agents[0].effective_llm(&config.llm);
        assert_eq!(a.interactive_max_rounds, 8, "per-agent override wins");
        let b = config.agents[1].effective_llm(&config.llm);
        assert_eq!(b.interactive_max_rounds, 24, "unset inherits global");
    }

    #[test]
    fn monitor_auto_disable_defaults_and_parses_off() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.monitor.auto_disable_after_failures, 30);

        let config =
            WorkforceConfig::from_toml("[monitor]\nauto_disable_after_failures = 0").unwrap();
        assert_eq!(config.monitor.auto_disable_after_failures, 0, "0 = off");
    }

    #[test]
    fn monitor_source_polling_defaults_and_parses() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(
            (
                config.monitor.min_poll_secs,
                config.monitor.poll_tick_secs,
                config.monitor.max_items
            ),
            (60, 15, 500)
        );
        let config = WorkforceConfig::from_toml(
            "[monitor]\nmin_poll_secs = 30\npoll_tick_secs = 5\nmax_items = 50",
        )
        .unwrap();
        assert_eq!(
            (
                config.monitor.min_poll_secs,
                config.monitor.poll_tick_secs,
                config.monitor.max_items
            ),
            (30, 5, 50)
        );
    }

    #[test]
    fn coding_session_config_uses_default_when_exposed_tools_empty() {
        let cs = CodingSessionConfig::default();
        assert_eq!(
            cs.effective_exposed_tools(),
            DEFAULT_CODING_SESSION_EXPOSED_TOOLS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );
        for bare in ["memory", "notes"] {
            assert!(
                DEFAULT_CODING_SESSION_EXPOSED_TOOLS.contains(&bare),
                "{bare} must be exposed to coding.session by default"
            );
        }

        let toml = r#"
[coding_session]
exposed_tools = ["github.*", "memory"]
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(
            config.coding_session.effective_exposed_tools(),
            vec!["github.*", "memory"]
        );
    }

    #[test]
    fn vision_config_global_inherits_when_agent_unset() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.vision]
model = "openrouter:anthropic:cheap"

[[agents]]
id = "alice"
[agents.llm]
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert_eq!(
            effective.vision.unwrap().model,
            "openrouter:anthropic:cheap"
        );
    }

    #[test]
    fn vision_config_agent_overrides_global() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.vision]
model = "openrouter:anthropic:cheap"

[[agents]]
id = "bob"
[agents.llm.vision]
model = "anthropic:standard"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert_eq!(effective.vision.unwrap().model, "anthropic:standard");
    }

    #[test]
    fn vision_config_no_global_no_agent_is_none() {
        let toml = r#"
[llm]
provider = "anthropic"

[[agents]]
id = "carol"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert!(effective.vision.is_none());
    }

    #[test]
    fn pacing_config_global_inherits_when_agent_unset() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.pacing]
enabled = true
tpm_limit = 500000

[[agents]]
id = "alice"
[agents.llm]
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert!(effective.pacing.enabled);
        assert_eq!(effective.pacing.tpm_limit, 500_000);
    }

    #[test]
    fn pacing_config_agent_override_disables_global() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.pacing]
enabled = true
tpm_limit = 500000

[[agents]]
id = "allan"
[agents.llm.pacing]
enabled = false
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert!(
            !effective.pacing.enabled,
            "per-agent [llm.pacing] should fully override the global policy"
        );
    }

    #[test]
    fn transcription_config_global_inherits_when_agent_unset() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.transcription]
model = "openrouter:google:cheap"

[[agents]]
id = "alice"
[agents.llm]
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert_eq!(
            effective.transcription.unwrap().model,
            "openrouter:google:cheap"
        );
    }

    #[test]
    fn transcription_config_agent_overrides_global() {
        let toml = r#"
[llm]
provider = "anthropic"
[llm.transcription]
model = "openrouter:google:cheap"

[[agents]]
id = "bob"
[agents.llm.transcription]
model = "groq:standard"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert_eq!(effective.transcription.unwrap().model, "groq:standard");
    }

    #[test]
    fn transcription_config_no_global_no_agent_is_none() {
        let toml = r#"
[llm]
provider = "anthropic"

[[agents]]
id = "carol"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        let effective = config.agents[0].effective_llm(&config.llm);
        assert!(effective.transcription.is_none());
    }

    #[test]
    fn parse_minimal_config() {
        // Save and temporarily unset env vars that would override defaults
        let nats_url = std::env::var("WORKFORCE_NATS_URL").ok();
        let listen = std::env::var("WORKFORCE_LISTEN").ok();
        std::env::remove_var("WORKFORCE_NATS_URL");
        std::env::remove_var("WORKFORCE_LISTEN");

        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.nats.url, "nats://localhost:4222");
        assert_eq!(config.llm.provider, "openrouter");
        assert_eq!(config.hub.listen, "0.0.0.0:8090");
        assert!(config.timezone.is_none());

        // Restore env vars
        if let Some(url) = nats_url {
            std::env::set_var("WORKFORCE_NATS_URL", url);
        }
        if let Some(addr) = listen {
            std::env::set_var("WORKFORCE_LISTEN", addr);
        }
    }

    #[test]
    fn parse_full_config() {
        // Save and temporarily unset env vars that would override config values
        let listen = std::env::var("WORKFORCE_LISTEN").ok();
        std::env::remove_var("WORKFORCE_LISTEN");

        let toml = r#"
timezone = "Europe/London"

[hub]
listen = "127.0.0.1:9090"
log_level = "debug"

[nats]
url = "nats://nats.example.com:4222"

[llm]
provider = "anthropic"
api_key = "sk-test-key"
default_model = "claude-opus-4"
max_tokens = 4096

[budget]
hub_daily_usd = 100.0
identity_daily_usd = 25.0
# Pre-overhaul keys — must be ignored, not resurrected as live ceilings.
daily_limit = 999.0
per_agent_limit = 999.0

[budget.agents.arch]
agent_daily_usd = 5.0

[[teams]]
id = "dev"
name = "Dev Team"
lead = "arch"
pattern = "pipeline"
require_review = true

[[teams.members]]
id = "arch"
role = "lead"
capabilities = ["design"]

[[teams.members]]
id = "dev1"
role = "developer"
model = "claude-sonnet-4"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.hub.listen, "127.0.0.1:9090");
        assert_eq!(config.llm.api_key, Some("sk-test-key".to_string()));
        assert_eq!(config.teams.len(), 1);
        assert_eq!(config.teams[0].members.len(), 2);
        assert_eq!(config.budget.hub_daily_usd, 100.0);
        assert_eq!(config.budget.identity_daily_usd, 25.0);
        // Defaults survive alongside explicit keys; legacy keys are inert.
        assert!(config.budget.enabled);
        assert_eq!(config.budget.agent_daily_usd, 200.0);
        assert_eq!(config.budget.for_agent("arch").agent_daily_usd, 5.0);
        assert_eq!(config.budget.for_agent("other").agent_daily_usd, 200.0);
        assert_eq!(config.timezone, Some("Europe/London".to_string()));

        // Restore env var
        if let Some(addr) = listen {
            std::env::set_var("WORKFORCE_LISTEN", addr);
        }
    }

    #[test]
    fn weaviate_api_key_env_fallback() {
        // Save and clear both names so the test starts from a known state.
        let prefixed = std::env::var("WORKFORCE_WEAVIATE_API_KEY").ok();
        let bare = std::env::var("WEAVIATE_API_KEY").ok();
        std::env::remove_var("WORKFORCE_WEAVIATE_API_KEY");
        std::env::remove_var("WEAVIATE_API_KEY");

        // Bare WEAVIATE_API_KEY should be picked up when the prefixed one is unset.
        std::env::set_var("WEAVIATE_API_KEY", "bare-key");
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.weaviate.api_key, Some("bare-key".to_string()));

        // Prefixed name still wins when both are set.
        std::env::set_var("WORKFORCE_WEAVIATE_API_KEY", "prefixed-key");
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.weaviate.api_key, Some("prefixed-key".to_string()));

        // Restore.
        std::env::remove_var("WORKFORCE_WEAVIATE_API_KEY");
        std::env::remove_var("WEAVIATE_API_KEY");
        if let Some(v) = prefixed {
            std::env::set_var("WORKFORCE_WEAVIATE_API_KEY", v);
        }
        if let Some(v) = bare {
            std::env::set_var("WEAVIATE_API_KEY", v);
        }
    }

    #[test]
    fn validate_empty_nats_url() {
        let mut config = WorkforceConfig::default();
        config.nats.url = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_duplicate_team_ids() {
        let config = WorkforceConfig::from_toml(
            r#"
[[teams]]
id = "team1"
name = "Team 1"
lead = "a"
[[teams.members]]
id = "a"
role = "dev"

[[teams]]
id = "team1"
name = "Team 1 Duplicate"
lead = "b"
[[teams.members]]
id = "b"
role = "dev"
"#,
        )
        .unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_toml_parses() {
        let toml = WorkforceConfig::default_toml();
        let config = WorkforceConfig::from_toml(&toml).unwrap();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn vellev_defaults_hold_without_toml_and_select_by_backend() {
        let unset = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(unset.vellev.url, "http://localhost:18080");
        assert_eq!(unset.vellev.tenant, "", "no tenant by default");
        assert!(unset.vellev.api_key.is_none());
        assert_eq!(
            unset.vellev.connect_timeout_secs,
            crate::postgres::default_backend_connect_timeout_secs()
        );
        assert!(!unset.memory.is_vellev());
        let set = WorkforceConfig::from_toml(
            "[memory]\nbackend = \"vellev\"\n[vellev]\nurl = \"http://vellev:8080\"\ntenant = \"harbor\"\napi_key = \"k\"\nconnect_timeout_secs = 12",
        )
        .unwrap();
        assert!(set.memory.is_vellev());
        assert_eq!(
            (set.vellev.url.as_str(), set.vellev.tenant.as_str()),
            ("http://vellev:8080", "harbor")
        );
        assert_eq!(set.vellev.connect_timeout_secs, 12);
        assert!(set.validate().is_ok());
    }

    #[test]
    fn the_vellev_backend_without_a_key_or_a_tenant_is_refused() {
        let keyless = WorkforceConfig::from_toml(
            "[memory]\nbackend = \"vellev\"\n[vellev]\ntenant = \"harbor\"",
        )
        .unwrap();
        let refused = keyless.validate().unwrap_err().to_string();
        assert!(refused.contains("[vellev] api_key"), "{refused}");
        let tenantless =
            WorkforceConfig::from_toml("[memory]\nbackend = \"vellev\"\n[vellev]\napi_key = \"k\"")
                .unwrap();
        let refused = tenantless.validate().unwrap_err().to_string();
        assert!(refused.contains("[vellev] tenant"), "{refused}");
    }

    /// vellev's own CLI reads VELLEV_API_KEY as an admin key; the hub reads
    /// only its own prefixed names. The environment is the test's own, so
    /// no other test sees it.
    #[test]
    fn only_workforce_vellev_variables_reach_the_vellev_config() {
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(set, _)| *set == name)
                    .map(|(_, value)| value.to_string())
                    .ok_or(std::env::VarError::NotPresent)
            }
        };
        let mut plain = WorkforceConfig::default();
        plain.apply_env_overrides_from(&environment(&[
            ("VELLEV_API_KEY", "an-admin-key"),
            ("VELLEV_URL", "http://admin-only:9"),
        ]));
        assert!(plain.vellev.api_key.is_none());
        assert_eq!(plain.vellev.url, "http://localhost:18080");
        let mut prefixed = WorkforceConfig::default();
        prefixed.apply_env_overrides_from(&environment(&[
            ("WORKFORCE_VELLEV_API_KEY", "the-hubs-key"),
            ("WORKFORCE_VELLEV_TENANT", "harbor"),
            ("WORKFORCE_VELLEV_URL", "http://vellev:18080"),
        ]));
        assert_eq!(prefixed.vellev.api_key.as_deref(), Some("the-hubs-key"));
        assert_eq!(prefixed.vellev.tenant, "harbor");
        assert_eq!(prefixed.vellev.url, "http://vellev:18080");
    }

    /// Memories, the artifact index and code vectors are each kept where
    /// their own setting says: by default the two indexes stay on Weaviate
    /// when memories move to vellev.
    #[test]
    fn the_three_stores_are_chosen_apart() {
        let on = |toml: &str| {
            let config = WorkforceConfig::from_toml(toml).unwrap();
            (
                config.memory.is_vellev(),
                config.artifacts_indexed_in_vellev(),
                config.code_vectors_in_vellev(),
                config.kept_in_vellev(),
            )
        };
        assert_eq!(on(""), (false, false, false, None));
        assert_eq!(
            on("[memory]\nbackend = \"vellev\""),
            (true, false, false, Some("[memory] backend")),
            "memories alone"
        );
        assert_eq!(
            on("[artifacts]\nindex_backend = \"vellev\""),
            (false, true, false, Some("[artifacts] index_backend"))
        );
        assert_eq!(
            on("[knowledge]\nvector_backend = \"vellev\""),
            (false, false, true, Some("[knowledge] vector_backend"))
        );
        assert_eq!(
            on("[memory]\nbackend = \"vellev\"\n[artifacts]\nindex_backend = \"vellev\"\n[knowledge]\nvector_backend = \"vellev\""),
            (true, true, true, Some("[memory] backend"))
        );
        let s3 = WorkforceConfig::from_toml("[artifacts]\nbackend = \"s3\"").unwrap();
        assert_eq!(
            s3.artifacts.backend, "s3",
            "the artifact store's own backend"
        );
        assert!(!s3.artifacts_indexed_in_vellev());
    }

    /// Any store kept in vellev needs its key and tenant, and an index
    /// backend is one of the two there are.
    #[test]
    fn any_store_in_vellev_needs_a_key_and_a_tenant() {
        for chosen in [
            "[memory]\nbackend = \"vellev\"",
            "[artifacts]\nindex_backend = \"vellev\"",
            "[knowledge]\nvector_backend = \"vellev\"",
        ] {
            let setting = chosen.replace('\n', " ").replace(" = \"vellev\"", "");
            let keyless = WorkforceConfig::from_toml(chosen).unwrap();
            let refused = keyless.validate().unwrap_err().to_string();
            assert!(
                refused.contains(&setting) && refused.contains("[vellev] api_key"),
                "{refused}"
            );
            let tenantless =
                WorkforceConfig::from_toml(&format!("{chosen}\n[vellev]\napi_key = \"k\""))
                    .unwrap();
            let refused = tenantless.validate().unwrap_err().to_string();
            assert!(refused.contains("[vellev] tenant"), "{refused}");
            let whole = format!("{chosen}\n[vellev]\napi_key = \"k\"\ntenant = \"acme\"");
            WorkforceConfig::from_toml(&whole)
                .unwrap()
                .validate()
                .unwrap();
        }
        WorkforceConfig::from_toml("").unwrap().validate().unwrap();
        for unknown in [
            "[artifacts]\nindex_backend = \"auto\"",
            "[knowledge]\nvector_backend = \"sqlite\"",
        ] {
            let refused = WorkforceConfig::from_toml(unknown)
                .unwrap()
                .validate()
                .unwrap_err()
                .to_string();
            assert!(
                refused.contains("must be \"weaviate\" or \"vellev\""),
                "{refused}"
            );
        }
    }

    /// An index left on Weaviate, which is off, while something else moved
    /// to vellev is said; health reports say where each store is.
    #[test]
    fn an_index_left_nowhere_is_said_and_health_says_where_each_store_is() {
        let config = |toml: &str| WorkforceConfig::from_toml(toml).unwrap();
        let plain = config("");
        assert!(plain.indexes_left_nowhere().is_empty(), "nothing moved");
        assert_eq!(
            plain.store_backends(),
            [
                ("memories", "sqlite".to_string()),
                ("artifact_index", "none".to_string()),
                ("code_vectors", "none".to_string())
            ]
        );
        let memories_only = config("[memory]\nbackend = \"vellev\"");
        let left = memories_only.indexes_left_nowhere();
        assert_eq!(left.len(), 2);
        assert!(left[0].contains("[artifacts] index_backend") && left[0].contains("keyword only"));
        assert!(left[1].contains("[knowledge] vector_backend"));
        let with_weaviate = config("[memory]\nbackend = \"vellev\"\n[weaviate]\nenabled = true");
        assert!(with_weaviate.indexes_left_nowhere().is_empty());
        assert_eq!(with_weaviate.store_backends()[1].1, "weaviate");
        let all = config("[memory]\nbackend = \"vellev\"\n[artifacts]\nindex_backend = \"vellev\"\n[knowledge]\nvector_backend = \"vellev\"");
        assert!(all.indexes_left_nowhere().is_empty());
        assert_eq!(
            all.store_backends().map(|(_, backend)| backend),
            ["vellev", "vellev", "vellev"]
        );
    }

    #[test]
    fn a_vellev_url_that_is_no_address_is_refused() {
        for (url, good) in [
            ("http://vellev:18080", true),
            ("https://vellev.internal", true),
            ("vellev:18080", false),
            ("", false),
            ("http://", false),
            ("http:// vellev", false),
        ] {
            let toml = format!(
                "[memory]\nbackend = \"vellev\"\n[vellev]\napi_key = \"k\"\ntenant = \"acme\"\nurl = \"{url}\""
            );
            let checked = WorkforceConfig::from_toml(&toml).unwrap().validate();
            assert_eq!(checked.is_ok(), good, "{url:?}: {checked:?}");
        }
        let unused = "[vellev]\nurl = \"nonsense\"";
        WorkforceConfig::from_toml(unused)
            .unwrap()
            .validate()
            .unwrap();
    }

    /// A search that waits no time at all for the index would never use
    /// it: refused when the config is read, for either index.
    #[test]
    fn a_search_timeout_of_nothing_is_refused() {
        for setting in [
            "[artifacts]\nindex_search_timeout_ms",
            "[knowledge]\nvector_search_timeout_ms",
        ] {
            let checked = |ms: u64| {
                let toml = format!("{setting} = {ms}");
                WorkforceConfig::from_toml(&toml).unwrap().validate()
            };
            checked(1).unwrap();
            let refused = checked(0).unwrap_err().to_string();
            let named = setting.replace('\n', " ");
            assert!(refused.contains(&named), "{refused}");
        }
    }

    /// `[knowledge] vector_standby`: none unless set, in the file or by
    /// its own variable.
    #[test]
    fn the_code_vector_standby_is_none_unless_set() {
        assert_eq!(WorkforceConfig::default().knowledge.vector_standby, "");
        let parsed: KnowledgeConfig = toml::from_str("vector_standby = \"vellev\"").unwrap();
        assert_eq!(parsed.vector_standby, "vellev");
        let environment = |name: &str| match name {
            "WORKFORCE_KNOWLEDGE_VECTOR_STANDBY" => Ok("weaviate".to_string()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let mut config = WorkforceConfig::default();
        config.apply_env_overrides_from(&environment);
        assert_eq!(config.knowledge.vector_standby, "weaviate");
    }

    #[test]
    fn the_index_backends_are_overridden_by_their_own_variables() {
        let environment = |name: &str| match name {
            "WORKFORCE_ARTIFACTS_INDEX_BACKEND" | "WORKFORCE_KNOWLEDGE_VECTOR_BACKEND" => {
                Ok("vellev".to_string())
            }
            _ => Err(std::env::VarError::NotPresent),
        };
        let mut config = WorkforceConfig::default();
        config.apply_env_overrides_from(&environment);
        assert!(config.artifacts_indexed_in_vellev() && config.code_vectors_in_vellev());
        assert!(!config.memory.is_vellev());
    }

    #[test]
    fn the_vellev_key_never_shows_in_debug() {
        let config = VellevConfig {
            api_key: Some("very-secret".into()),
            ..Default::default()
        };
        let shown = format!("{config:?}");
        assert!(
            shown.contains("<redacted>") && !shown.contains("very-secret"),
            "{shown}"
        );
    }

    #[test]
    fn validate_valid_timezone() {
        let config = WorkforceConfig::from_toml("timezone = \"America/New_York\"").unwrap();
        assert!(config.validate().is_ok());
        assert_eq!(config.user_timezone().to_string(), "America/New_York");
    }

    #[test]
    fn validate_invalid_timezone() {
        let config = WorkforceConfig::from_toml("timezone = \"Mars/Olympus\"").unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn user_timezone_defaults_to_utc() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert_eq!(config.user_timezone(), chrono_tz::UTC);
    }

    #[test]
    fn user_timezone_resolves_correctly() {
        let config = WorkforceConfig::from_toml("timezone = \"Asia/Tokyo\"").unwrap();
        let tz = config.user_timezone();
        assert_eq!(tz.to_string(), "Asia/Tokyo");
    }

    /// `timezone` is a root-level key, so it has to appear above the first
    /// `[section]` in the file. Put below one it becomes that section's key,
    /// parses without complaint, and silently leaves every agent on UTC —
    /// which is what happened in production, and reads as the assistant being
    /// an hour wrong rather than misconfigured.
    #[test]
    fn a_timezone_under_a_section_is_not_the_root_key() {
        let correct = WorkforceConfig::from_toml("timezone = \"Europe/London\"\n[hub]\n")
            .expect("root-level parses");
        assert_eq!(correct.user_timezone().to_string(), "Europe/London");

        let misplaced = WorkforceConfig::from_toml("[hub]\ntimezone = \"Europe/London\"\n")
            .expect("still valid TOML, which is the trap");
        assert_eq!(
            misplaced.user_timezone().to_string(),
            "UTC",
            "a misplaced key must not look like it worked"
        );
    }

    /// An unparseable zone falls back rather than failing, so a typo is a wrong
    /// clock and not a hub that will not start.
    #[test]
    fn an_invalid_timezone_falls_back_to_utc() {
        let config = WorkforceConfig::from_toml("timezone = \"Europe/Lundon\"").unwrap();
        assert_eq!(config.user_timezone().to_string(), "UTC");
    }

    // ─── Session Policies Tests ──────────────────────────────────────────

    #[test]
    fn resolve_policy_global_fallback() {
        let policies = SessionPoliciesConfig::default();
        let global = SessionLifecycleConfig {
            daily_reset_hour: Some(4),
            idle_expiry_minutes: Some(60),
            archive_expired: true,
            check_interval_secs: 300,
        };

        let resolved = policies.resolve("slack", None, &global);
        assert!(resolved.daily_reset);
        assert_eq!(resolved.reset_hour, Some(4));
        assert_eq!(resolved.idle_expiry_minutes, 60);
    }

    #[test]
    fn resolve_policy_global_no_daily_reset() {
        let policies = SessionPoliciesConfig::default();
        let global = SessionLifecycleConfig {
            daily_reset_hour: None,
            idle_expiry_minutes: Some(30),
            ..Default::default()
        };

        let resolved = policies.resolve("cli", None, &global);
        assert!(!resolved.daily_reset);
        assert_eq!(resolved.reset_hour, None);
        assert_eq!(resolved.idle_expiry_minutes, 30);
    }

    #[test]
    fn resolve_policy_type_override() {
        use crate::session::SessionContextType;

        let mut policies = SessionPoliciesConfig::default();
        policies.types.insert(
            SessionContextType::Thread,
            ResetPolicy {
                daily_reset: Some(false),
                reset_hour: None,
                idle_expiry_minutes: Some(15),
            },
        );

        let global = SessionLifecycleConfig {
            daily_reset_hour: Some(4),
            idle_expiry_minutes: Some(60),
            ..Default::default()
        };

        // Thread session: type override applies
        let resolved = policies.resolve("slack", Some(&SessionContextType::Thread), &global);
        assert!(!resolved.daily_reset);
        assert_eq!(resolved.reset_hour, None); // daily_reset=false → no hour
        assert_eq!(resolved.idle_expiry_minutes, 15);

        // DM session: no type override → falls back to global
        let resolved = policies.resolve("slack", Some(&SessionContextType::Dm), &global);
        assert!(resolved.daily_reset);
        assert_eq!(resolved.reset_hour, Some(4));
        assert_eq!(resolved.idle_expiry_minutes, 60);
    }

    #[test]
    fn resolve_policy_channel_override() {
        let mut policies = SessionPoliciesConfig::default();
        policies.channels.insert(
            "cli".to_string(),
            ResetPolicy {
                daily_reset: Some(false),
                reset_hour: None,
                idle_expiry_minutes: Some(0),
            },
        );

        let global = SessionLifecycleConfig {
            daily_reset_hour: Some(4),
            idle_expiry_minutes: Some(60),
            ..Default::default()
        };

        // CLI channel: override applies
        let resolved = policies.resolve("cli", None, &global);
        assert!(!resolved.daily_reset);
        assert_eq!(resolved.idle_expiry_minutes, 0);

        // Slack channel: no override → global
        let resolved = policies.resolve("slack", None, &global);
        assert!(resolved.daily_reset);
        assert_eq!(resolved.idle_expiry_minutes, 60);
    }

    #[test]
    fn resolve_policy_channel_overrides_type() {
        use crate::session::SessionContextType;

        let mut policies = SessionPoliciesConfig::default();
        policies.types.insert(
            SessionContextType::Dm,
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(30),
            },
        );
        policies.channels.insert(
            "cli".to_string(),
            ResetPolicy {
                daily_reset: None,
                reset_hour: None,
                idle_expiry_minutes: Some(120),
            },
        );

        let global = SessionLifecycleConfig {
            daily_reset_hour: None,
            idle_expiry_minutes: Some(60),
            ..Default::default()
        };

        // CLI + DM: channel override (120) takes precedence over type (30)
        let resolved = policies.resolve("cli", Some(&SessionContextType::Dm), &global);
        assert_eq!(resolved.idle_expiry_minutes, 120);

        // Slack + DM: only type override (30)
        let resolved = policies.resolve("slack", Some(&SessionContextType::Dm), &global);
        assert_eq!(resolved.idle_expiry_minutes, 30);
    }

    #[test]
    fn resolve_policy_reset_hour_inheritance() {
        use crate::session::SessionContextType;

        let mut policies = SessionPoliciesConfig::default();
        policies.types.insert(
            SessionContextType::Group,
            ResetPolicy {
                daily_reset: None,   // Inherit from global (true)
                reset_hour: Some(6), // But reset at 6 AM instead of 4
                idle_expiry_minutes: None,
            },
        );

        let global = SessionLifecycleConfig {
            daily_reset_hour: Some(4),
            idle_expiry_minutes: Some(60),
            ..Default::default()
        };

        let resolved = policies.resolve("slack", Some(&SessionContextType::Group), &global);
        assert!(resolved.daily_reset); // Inherited from global
        assert_eq!(resolved.reset_hour, Some(6)); // Overridden by type
        assert_eq!(resolved.idle_expiry_minutes, 60); // Inherited from global
    }

    #[test]
    fn parse_session_policies_from_toml() {
        let toml = r#"
[session_lifecycle]
daily_reset_hour = 4
idle_expiry_minutes = 60

[session_policies.types.dm]
idle_expiry_minutes = 120

[session_policies.types.thread]
daily_reset = false
idle_expiry_minutes = 15

[session_policies.channels.cli]
daily_reset = false
idle_expiry_minutes = 0

[session_policies.channels.slack]
reset_hour = 6
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.session_lifecycle.daily_reset_hour, Some(4));
        assert_eq!(config.session_policies.types.len(), 2);
        assert_eq!(config.session_policies.channels.len(), 2);

        // Verify CLI channel policy
        let cli = config.session_policies.channels.get("cli").unwrap();
        assert_eq!(cli.daily_reset, Some(false));
        assert_eq!(cli.idle_expiry_minutes, Some(0));

        // Verify thread type policy
        use crate::session::SessionContextType;
        let thread = config
            .session_policies
            .types
            .get(&SessionContextType::Thread)
            .unwrap();
        assert_eq!(thread.daily_reset, Some(false));
        assert_eq!(thread.idle_expiry_minutes, Some(15));
    }

    #[test]
    fn heartbeat_defaults() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert!(!config.heartbeat.enabled);
        assert_eq!(config.heartbeat.interval_secs, 1800);
        assert_eq!(config.heartbeat.active_start, "08:00");
        assert_eq!(config.heartbeat.active_end, "23:00");
        assert!(config.heartbeat.channels.is_empty());
        assert_eq!(
            config.heartbeat.deliver,
            HeartbeatDeliverMode::ActionableOnly
        );
        assert_eq!(config.heartbeat.history_size, 20);
        assert!(config.heartbeat.prompt.is_none());
    }

    #[test]
    fn heartbeat_full_config() {
        let toml = r#"
[heartbeat]
enabled = true
interval_secs = 900
active_start = "09:00"
active_end = "22:00"
channels = ["slack", "ws"]
deliver = "all"
history_size = 50
prompt = "Check the weather"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert!(config.heartbeat.enabled);
        assert_eq!(config.heartbeat.interval_secs, 900);
        assert_eq!(config.heartbeat.active_start, "09:00");
        assert_eq!(config.heartbeat.active_end, "22:00");
        assert_eq!(config.heartbeat.channels, vec!["slack", "ws"]);
        assert_eq!(config.heartbeat.deliver, HeartbeatDeliverMode::All);
        assert_eq!(config.heartbeat.history_size, 50);
        assert_eq!(
            config.heartbeat.prompt.as_deref(),
            Some("Check the weather")
        );
    }

    // ─── DW-149: Access control tests ───────────────────────────────────

    fn allow_only(ids: &[&str]) -> AgentAccessConfig {
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: ids.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    fn block_only(ids: &[&str]) -> AgentAccessConfig {
        AgentAccessConfig {
            mode: AccessMode::Blocklist,
            block: ids.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn evaluate_open_allows_all() {
        let cfg = AgentAccessConfig::default();
        assert_eq!(cfg.evaluate(None, &[]), AccessDecision::Allow);
        assert_eq!(
            cfg.evaluate(Some("uuid"), &["slack:U1"]),
            AccessDecision::Allow
        );
    }

    #[test]
    fn evaluate_allowlist_matches_identity() {
        let cfg = allow_only(&["alice-uuid"]);
        assert_eq!(cfg.evaluate(Some("alice-uuid"), &[]), AccessDecision::Allow);
        assert!(matches!(
            cfg.evaluate(Some("bob-uuid"), &[]),
            AccessDecision::Deny {
                reason: "allowlist_no_match",
                ..
            }
        ));
    }

    #[test]
    fn evaluate_allowlist_matches_surface_id() {
        let cfg = allow_only(&["slack:U123"]);
        assert_eq!(cfg.evaluate(None, &["slack:U123"]), AccessDecision::Allow);
        assert!(matches!(
            cfg.evaluate(None, &["slack:U999"]),
            AccessDecision::Deny { .. }
        ));
    }

    #[test]
    fn evaluate_allowlist_denies_anonymous() {
        let cfg = allow_only(&["alice-uuid"]);
        assert!(matches!(
            cfg.evaluate(None, &[]),
            AccessDecision::Deny {
                reason: "allowlist_no_match",
                ..
            }
        ));
    }

    #[test]
    fn evaluate_blocklist_allows_anonymous() {
        let cfg = block_only(&["evil-uuid"]);
        assert_eq!(cfg.evaluate(None, &[]), AccessDecision::Allow);
    }

    #[test]
    fn evaluate_blocklist_denies_listed() {
        let cfg = block_only(&["evil-uuid", "slack:U_BAD"]);
        assert!(matches!(
            cfg.evaluate(Some("evil-uuid"), &[]),
            AccessDecision::Deny {
                reason: "blocklist_match",
                ..
            }
        ));
        assert!(matches!(
            cfg.evaluate(None, &["slack:U_BAD"]),
            AccessDecision::Deny {
                reason: "blocklist_match",
                ..
            }
        ));
        assert_eq!(cfg.evaluate(Some("good-uuid"), &[]), AccessDecision::Allow);
    }

    #[test]
    fn evaluate_carries_deny_message() {
        let cfg = AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec!["alice".into()],
            deny_message: Some("sorry".into()),
            ..Default::default()
        };
        match cfg.evaluate(Some("bob"), &[]) {
            AccessDecision::Deny { message, .. } => {
                assert_eq!(message.as_deref(), Some("sorry"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn normalize_migrates_bare_slack_ids() {
        let mut cfg = AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![
                "U01ABCDEFG".into(),
                "slack:U02HIJKLM".into(),
                "WBOSSBOSS".into(),
                "lowercase".into(),
                "U-with-dash".into(),
                "550e8400-e29b-41d4-a716-446655440000".into(),
            ],
            ..Default::default()
        };
        cfg.normalize_bare_slack_ids("test-agent");
        assert_eq!(
            cfg.allow,
            vec![
                "slack:U01ABCDEFG".to_string(),
                "slack:U02HIJKLM".to_string(),
                "slack:WBOSSBOSS".to_string(),
                "lowercase".to_string(),
                "U-with-dash".to_string(),
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
            ]
        );
    }

    #[test]
    fn normalize_short_bare_id_is_left_alone() {
        let mut cfg = AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec!["U1234".into()],
            ..Default::default()
        };
        cfg.normalize_bare_slack_ids("test-agent");
        assert_eq!(cfg.allow, vec!["U1234".to_string()]);
    }

    #[test]
    fn access_config_toml_parse() {
        let toml = r#"
            id = "test-agent"
            [access]
            mode = "allowlist"
            allow = ["U123", "U456"]
            deny_message = "Sorry, I can't help you."
        "#;
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.access.mode, AccessMode::Allowlist);
        assert_eq!(config.access.allow.len(), 2);
        assert_eq!(
            config.access.deny_message.as_deref(),
            Some("Sorry, I can't help you.")
        );
    }

    #[test]
    fn access_config_external_override_defaults_none() {
        let config: AgentConfig = toml::from_str("id = \"a\"\n[access]\n").unwrap();
        assert_eq!(config.access.allow_external_users, None);
    }

    #[test]
    fn access_config_external_override_parses() {
        let toml = "id = \"a\"\n[access]\nallow_external_users = true\n";
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.access.allow_external_users, Some(true));
    }

    // ─── DW-148: DM routing tests ──────────────────────────────────────

    #[test]
    fn dm_routing_defaults_empty() {
        let config = DmRoutingConfig::default();
        assert!(config.user_routes.is_empty());
        assert!(config.default_agent.is_none());
    }

    #[test]
    fn dm_routing_toml_parse() {
        let toml = r#"
            [dm_routing]
            default_agent = "allan"
            [dm_routing.user_routes]
            U123 = "architect"
            U456 = "allan"
        "#;
        let config: WorkforceConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.dm_routing.default_agent.as_deref(), Some("allan"));
        assert_eq!(
            config.dm_routing.user_routes.get("U123").unwrap(),
            "architect"
        );
    }

    // ─── DW-145: User map test ──────────────────────────────────────────

    #[test]
    fn user_map_toml_parse() {
        let toml = r#"
            id = "test-agent"
            [user_map]
            U123 = "kings"
            U456 = "alice"
        "#;
        let config: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.user_map.get("U123").unwrap(), "kings");
        assert_eq!(config.user_map.get("U456").unwrap(), "alice");
    }

    fn test_agent(memory: Option<AgentMemoryConfig>) -> AgentConfig {
        AgentConfig {
            id: "test".into(),
            name: None,
            model: None,
            workspace: None,
            system_prompt: None,
            capabilities: Vec::new(),
            default: false,
            llm: None,
            vault: None,
            compaction: None,
            memory,
            knowledge: None,
            access: AgentAccessConfig::default(),
            scope: None,
            user_map: Default::default(),
            owner_id: None,
            owner_type: Default::default(),
            advisor: None,
            advisor_mode: false,
        }
    }

    fn test_agent_with_advisor(advisor: Option<AgentAdvisorConfig>) -> AgentConfig {
        let mut a = test_agent(None);
        a.advisor = advisor;
        a
    }

    #[test]
    fn effective_advisor_inherits_global_when_unset() {
        let global = AdvisorConfig {
            enabled: true,
            source: AdvisorSource::ClaudeCode,
            max_rounds: 7,
            ..Default::default()
        };
        let agent = test_agent_with_advisor(None);
        let effective = agent.effective_advisor(&global);
        assert!(effective.enabled);
        assert_eq!(effective.source, AdvisorSource::ClaudeCode);
        assert_eq!(effective.max_rounds, 7);
    }

    #[test]
    fn effective_advisor_agent_overrides_global() {
        let global = AdvisorConfig {
            enabled: false,
            source: AdvisorSource::Model,
            cc_allow_failover_to_models: false,
            model: Some("expensive".into()),
            ..Default::default()
        };
        let agent = test_agent_with_advisor(Some(AgentAdvisorConfig {
            enabled: Some(true),
            source: Some(AdvisorSource::ClaudeCode),
            cc_allow_failover_to_models: Some(true),
            cc_fallback_model: Some("expensive".into()),
            ..Default::default()
        }));
        let effective = agent.effective_advisor(&global);
        assert!(effective.enabled);
        assert_eq!(effective.source, AdvisorSource::ClaudeCode);
        assert!(effective.cc_allow_failover_to_models);
        assert_eq!(effective.cc_fallback_model.as_deref(), Some("expensive"));
        // unset agent fields inherit global
        assert_eq!(effective.model.as_deref(), Some("expensive"));
    }

    #[test]
    fn effective_advisor_defaults_are_safe() {
        let cfg = AdvisorConfig::default();
        assert!(!cfg.enabled, "advisor must be opt-in");
        assert!(
            !cfg.cc_allow_failover_to_models,
            "CC cost-guard must default to OFF"
        );
        assert_eq!(cfg.payload, AdvisorPayload::Transcript);
        assert_eq!(cfg.source, AdvisorSource::Model);
        assert!(cfg.per_turn_invocation_limit > 0);
        assert!(
            cfg.max_rounds >= 7,
            "must leave headroom for premise verification + synthesis"
        );
        // Fan-out is available by default (the effective gate is the caller
        // passing lenses), with a sane width cap.
        assert!(cfg.fanout.enabled);
        assert!(cfg.fanout.max_lenses >= 2);
        assert!(cfg.fanout.synth_model.is_none());
    }

    #[test]
    fn effective_advisor_overrides_fanout_block_wholesale() {
        let global = AdvisorConfig {
            enabled: true,
            ..Default::default()
        };
        // Unset → inherits the global fan-out block.
        let inherited =
            test_agent_with_advisor(Some(AgentAdvisorConfig::default())).effective_advisor(&global);
        assert!(inherited.fanout.enabled);
        // Set → replaces the whole block.
        let overridden = test_agent_with_advisor(Some(AgentAdvisorConfig {
            fanout: Some(AdvisorFanoutConfig {
                enabled: false,
                max_lenses: 2,
                synth_model: Some("powerful".into()),
                ..Default::default()
            }),
            ..Default::default()
        }))
        .effective_advisor(&global);
        assert!(!overridden.fanout.enabled);
        assert_eq!(overridden.fanout.max_lenses, 2);
        assert_eq!(overridden.fanout.synth_model.as_deref(), Some("powerful"));
    }

    #[test]
    fn merge_advisor_workspace_override() {
        let base = AdvisorConfig {
            enabled: false,
            source: AdvisorSource::Model,
            model: Some("expensive".into()),
            ..Default::default()
        };
        let ws = AgentWorkspaceConfig {
            advisor: Some(AgentAdvisorConfig {
                enabled: Some(true),
                source: Some(AdvisorSource::Agent),
                agent_id: Some("reviewer".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = ws.merge_advisor(&base);
        assert!(merged.enabled);
        assert_eq!(merged.source, AdvisorSource::Agent);
        assert_eq!(merged.agent_id.as_deref(), Some("reviewer"));
        assert_eq!(merged.model.as_deref(), Some("expensive")); // inherited
    }

    #[test]
    fn merge_llm_workspace_pacing_override() {
        // This is the seam the pacing hot-reload uses: editing an agent's
        // workspace config.toml `[llm.pacing]` flows through `merge_llm`
        // (via `merged_effective_llm` in the hub). Assert a workspace override
        // wins, and that an absent override inherits the global policy.
        let base = LlmConfig {
            pacing: PacingConfig {
                enabled: true,
                tpm_limit: 2_000_000,
                ..Default::default()
            },
            ..Default::default()
        };

        // Workspace opts out (e.g. a high-quota direct-Gemini agent).
        let ws_off = AgentWorkspaceConfig {
            llm: Some(AgentLlmConfig {
                pacing: Some(PacingConfig {
                    enabled: false,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !ws_off.merge_llm(&base).pacing.enabled,
            "workspace [llm.pacing] should override the global policy"
        );

        // Workspace leaves pacing unset → inherit the global policy.
        let ws_inherit = AgentWorkspaceConfig {
            llm: Some(AgentLlmConfig::default()),
            ..Default::default()
        };
        let merged = ws_inherit.merge_llm(&base);
        assert!(merged.pacing.enabled);
        assert_eq!(merged.pacing.tpm_limit, 2_000_000);
    }

    #[test]
    fn advisor_config_toml_parse() {
        let toml_input = r#"
[advisor]
enabled = true
source = "claude_code"
payload = "transcript"
model = "opus"
cc_allow_failover_to_models = false
max_rounds = 9
"#;
        let cfg: toml::Table = toml::from_str(toml_input).unwrap();
        let advisor: AdvisorConfig = cfg["advisor"].clone().try_into().unwrap();
        assert!(advisor.enabled);
        assert_eq!(advisor.source, AdvisorSource::ClaudeCode);
        assert_eq!(advisor.payload, AdvisorPayload::Transcript);
        assert_eq!(advisor.model.as_deref(), Some("opus"));
        assert!(!advisor.cc_allow_failover_to_models);
        assert_eq!(advisor.max_rounds, 9);
    }

    #[test]
    fn advisor_config_parses_legacy_cc_max_turns_alias() {
        let toml_input = r#"
[advisor]
enabled = true
cc_max_turns = 12
"#;
        let cfg: toml::Table = toml::from_str(toml_input).unwrap();
        let advisor: AdvisorConfig = cfg["advisor"].clone().try_into().unwrap();
        assert_eq!(
            advisor.max_rounds, 12,
            "legacy cc_max_turns key must alias onto max_rounds"
        );
    }

    #[test]
    fn effective_memory_inherits_global() {
        let global = MemoryConfig {
            passive: true,
            passive_min_turn_tokens: 300,
            consolidation: true,
            consolidation_interval_secs: 7200,
            ..Default::default()
        };
        let agent = test_agent(None);
        let effective = agent.effective_memory(&global);
        assert!(effective.passive);
        assert_eq!(effective.passive_min_turn_tokens, 300);
        assert!(effective.consolidation);
        assert_eq!(effective.consolidation_interval_secs, 7200);
    }

    #[test]
    fn effective_memory_agent_override() {
        let global = MemoryConfig {
            passive: true,
            passive_min_turn_tokens: 200,
            consolidation: true,
            ..Default::default()
        };
        let agent = test_agent(Some(AgentMemoryConfig {
            passive: Some(false),
            passive_min_turn_tokens: Some(500),
            ..Default::default()
        }));
        let effective = agent.effective_memory(&global);
        assert!(!effective.passive);
        assert_eq!(effective.passive_min_turn_tokens, 500);
        assert!(effective.consolidation);
    }

    #[test]
    fn recall_min_certainty_defaults_to_none() {
        let global = MemoryConfig::default();
        let agent = test_agent(None);
        let effective = agent.effective_memory(&global);
        assert_eq!(effective.recall_min_certainty, None);
    }

    #[test]
    fn recall_min_certainty_global_propagates() {
        let global = MemoryConfig {
            recall_min_certainty: Some(0.65),
            ..Default::default()
        };
        let agent = test_agent(None);
        let effective = agent.effective_memory(&global);
        assert_eq!(effective.recall_min_certainty, Some(0.65));
    }

    #[test]
    fn recall_min_certainty_agent_overrides_global() {
        let global = MemoryConfig {
            recall_min_certainty: Some(0.65),
            ..Default::default()
        };
        let agent = test_agent(Some(AgentMemoryConfig {
            recall_min_certainty: Some(0.8),
            ..Default::default()
        }));
        let effective = agent.effective_memory(&global);
        assert_eq!(effective.recall_min_certainty, Some(0.8));
    }

    #[test]
    fn merge_memory_workspace_override() {
        let base = MemoryConfig {
            passive: false,
            passive_model: Some("cheap".into()),
            consolidation_interval_secs: 14400,
            ..Default::default()
        };
        let ws = AgentWorkspaceConfig {
            memory: Some(AgentMemoryConfig {
                passive: Some(true),
                passive_model: Some("fast".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = ws.merge_memory(&base);
        assert!(merged.passive);
        assert_eq!(merged.passive_model.as_deref(), Some("fast"));
        assert_eq!(merged.consolidation_interval_secs, 14400); // inherited
    }

    #[test]
    fn recall_recency_boost_defaults_and_toml_parse() {
        let defaults = MemoryConfig::default();
        assert_eq!(defaults.recall_recency_boost, 0.25);
        assert_eq!(defaults.recall_recency_half_life_hours, 6.0);

        // A [memory] TOML without the fields parses to the defaults.
        let cfg: MemoryConfig = toml::from_str(r#"passive = true"#).expect("memory toml parses");
        assert_eq!(cfg.recall_recency_boost, 0.25);
        assert_eq!(cfg.recall_recency_half_life_hours, 6.0);
    }

    #[test]
    fn recall_recency_boost_agent_section_merge() {
        let global = MemoryConfig::default();
        let agent = test_agent(Some(AgentMemoryConfig {
            recall_recency_boost: Some(0.5),
            recall_recency_half_life_hours: Some(2.0),
            ..Default::default()
        }));
        let effective = agent.effective_memory(&global);
        assert_eq!(effective.recall_recency_boost, 0.5);
        assert_eq!(effective.recall_recency_half_life_hours, 2.0);

        // Unset agent fields inherit global.
        let inherit = test_agent(Some(AgentMemoryConfig::default())).effective_memory(&global);
        assert_eq!(inherit.recall_recency_boost, 0.25);
        assert_eq!(inherit.recall_recency_half_life_hours, 6.0);
    }

    #[test]
    fn recall_recency_boost_workspace_merge_wins() {
        let base = MemoryConfig::default();
        let ws = AgentWorkspaceConfig {
            memory: Some(AgentMemoryConfig {
                recall_recency_boost: Some(0.0),
                recall_recency_half_life_hours: Some(1.0),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = ws.merge_memory(&base);
        assert_eq!(merged.recall_recency_boost, 0.0);
        assert_eq!(merged.recall_recency_half_life_hours, 1.0);
    }

    #[test]
    fn sessions_config_defaults() {
        // An empty `[sessions]` TOML (and the Default impl) yields the digest
        // defaults: enabled, 3 sessions, 12h window, 1200 chars.
        let cfg: SessionsConfig = toml::from_str("").expect("sessions toml parses");
        assert_eq!(cfg.backend, "jsonl");
        assert!(cfg.digest_enabled);
        assert_eq!(cfg.digest_max_sessions, 3);
        assert_eq!(cfg.digest_window_hours, 12);
        assert_eq!(cfg.digest_max_chars, 1200);

        let digest = SessionsConfig::default().digest_settings();
        assert!(digest.enabled);
        assert_eq!(digest.max_sessions, 3);
        assert_eq!(digest.window_hours, 12);
        assert_eq!(digest.max_chars, 1200);
    }

    #[test]
    fn agent_recall_scope_defaults_to_shared() {
        assert_eq!(
            MemoryConfig::default().agent_recall_scope,
            AgentRecallScope::Shared
        );
        assert!(!AgentRecallScope::Shared.owned_only());
        assert!(AgentRecallScope::Isolated.owned_only());
    }

    #[test]
    fn agent_recall_scope_parses_from_toml() {
        let cfg: MemoryConfig =
            toml::from_str(r#"agent_recall_scope = "isolated""#).expect("memory toml parses");
        assert_eq!(cfg.agent_recall_scope, AgentRecallScope::Isolated);
    }

    #[test]
    fn agent_recall_scope_agent_override_wins() {
        let global = MemoryConfig {
            agent_recall_scope: AgentRecallScope::Shared,
            ..Default::default()
        };
        let agent = test_agent(Some(AgentMemoryConfig {
            agent_recall_scope: Some(AgentRecallScope::Isolated),
            ..Default::default()
        }));
        assert_eq!(
            agent.effective_memory(&global).agent_recall_scope,
            AgentRecallScope::Isolated
        );
    }

    #[test]
    fn agent_recall_scope_inherits_global_when_unset() {
        let global = MemoryConfig {
            agent_recall_scope: AgentRecallScope::Isolated,
            ..Default::default()
        };
        let agent = test_agent(None);
        assert_eq!(
            agent.effective_memory(&global).agent_recall_scope,
            AgentRecallScope::Isolated
        );
    }

    #[test]
    fn memory_config_toml_parse() {
        let toml = r#"
[memory]
passive = true
passive_model = "cheap"
passive_min_turn_tokens = 100
consolidation = true
consolidation_interval_secs = 3600
        "#;
        let config: toml::Value = toml::from_str(toml).unwrap();
        let mem: MemoryConfig = config["memory"].clone().try_into().unwrap();
        assert!(mem.passive);
        assert_eq!(mem.passive_model.as_deref(), Some("cheap"));
        assert_eq!(mem.passive_min_turn_tokens, 100);
        assert!(mem.consolidation);
        assert_eq!(mem.consolidation_interval_secs, 3600);
    }

    #[test]
    fn summariser_config_defaults() {
        let config: KnowledgeSummariserConfig = toml::from_str("").unwrap();
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.url, "http://localhost:11434");
        assert_eq!(config.model, "qwen2.5-coder:7b");
        assert!(config.api_key.is_none());
    }

    #[test]
    fn summariser_config_ollama_url_alias() {
        let config: KnowledgeSummariserConfig =
            toml::from_str(r#"ollama_url = "http://ollama:11434""#).unwrap();
        assert_eq!(config.url, "http://ollama:11434");
    }

    #[test]
    fn summariser_config_url_field() {
        let config: KnowledgeSummariserConfig =
            toml::from_str(r#"url = "https://my-vllm.example.com""#).unwrap();
        assert_eq!(config.url, "https://my-vllm.example.com");
    }

    #[test]
    fn summariser_config_full() {
        let toml = r#"
provider = "anthropic"
url = "https://api.anthropic.com"
model = "claude-haiku-4-5-20251001"
api_key = "sk-ant-test"
"#;
        let config: KnowledgeSummariserConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.url, "https://api.anthropic.com");
        assert_eq!(config.model, "claude-haiku-4-5-20251001");
        assert_eq!(config.api_key.as_deref(), Some("sk-ant-test"));
    }

    // -- Env-var expansion in workforce.toml (post-parse, pre-deserialize) --

    #[test]
    fn from_toml_expands_env_var_in_string_field() {
        std::env::set_var("WF_CFG_TEST_LISTEN", "127.0.0.1:9999");
        let toml = r#"
[hub]
listen = "${WF_CFG_TEST_LISTEN}"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.hub.listen, "127.0.0.1:9999");
        std::env::remove_var("WF_CFG_TEST_LISTEN");
    }

    #[test]
    fn from_toml_expands_env_var_with_default() {
        std::env::remove_var("WF_CFG_TEST_LOG_LEVEL_NEVER_SET");
        let toml = r#"
[hub]
log_level = "${WF_CFG_TEST_LOG_LEVEL_NEVER_SET:-debug}"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.hub.log_level, "debug");
    }

    #[test]
    fn from_toml_expands_env_var_in_nested_section() {
        std::env::set_var("WF_CFG_TEST_OIDC_CID", "test-client-id-1234");
        let toml = r#"
[gateway.oidc]
enabled = true
display_name = "Test"
issuer_url = "https://accounts.google.com"
client_id = "${WF_CFG_TEST_OIDC_CID}"
client_secret = "static-secret-for-test"
redirect_uri = "https://example.com/cb"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(
            config.gateway.oidc.as_ref().unwrap().client_id,
            "test-client-id-1234"
        );
        std::env::remove_var("WF_CFG_TEST_OIDC_CID");
    }

    #[test]
    fn from_toml_env_var_no_op_when_no_placeholders() {
        // Plain config without any ${...} placeholders deserializes unchanged.
        let listen = std::env::var("WORKFORCE_LISTEN").ok();
        std::env::remove_var("WORKFORCE_LISTEN");

        let toml = r#"
[hub]
listen = "0.0.0.0:8090"
log_level = "info"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.hub.listen, "0.0.0.0:8090");
        assert_eq!(config.hub.log_level, "info");

        if let Some(addr) = listen {
            std::env::set_var("WORKFORCE_LISTEN", addr);
        }
    }

    // -- Postgres-default backend injection --

    #[test]
    fn postgres_enabled_defaults_all_stores_to_postgres() {
        let toml = r#"
[postgres]
enabled = true
url = "postgres://wf:secret@db:5432/workforce"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.telemetry.backend, "postgres");
        assert_eq!(config.identity.backend, "postgres");
        assert_eq!(config.pairing_store.backend, "postgres");
        assert_eq!(config.codegraph.backend, "postgres");
    }

    #[test]
    fn postgres_enabled_respects_explicit_sqlite_per_store() {
        let toml = r#"
[postgres]
enabled = true
url = "postgres://wf:secret@db:5432/workforce"

[telemetry]
backend = "sqlite"

[identity]
backend = "sqlite"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        // Explicit sqlite preserved
        assert_eq!(config.telemetry.backend, "sqlite");
        assert_eq!(config.identity.backend, "sqlite");
        // Unset stores get the postgres default
        assert_eq!(config.pairing_store.backend, "postgres");
        assert_eq!(config.codegraph.backend, "postgres");
    }

    #[test]
    fn postgres_disabled_keeps_sqlite_defaults() {
        let toml = r#"
[postgres]
enabled = false
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.telemetry.backend, "sqlite");
        assert_eq!(config.identity.backend, "sqlite");
        assert_eq!(config.pairing_store.backend, "sqlite");
        assert_eq!(config.codegraph.backend, "sqlite");
    }

    #[test]
    fn no_postgres_section_keeps_sqlite_defaults() {
        // A pre-postgres-feature workforce.toml stays on SQLite.
        let toml = r#"
[hub]
listen = "0.0.0.0:8090"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert_eq!(config.telemetry.backend, "sqlite");
        assert_eq!(config.identity.backend, "sqlite");
        assert_eq!(config.pairing_store.backend, "sqlite");
        assert_eq!(config.codegraph.backend, "sqlite");
    }

    #[test]
    fn aws_s3_defaults_when_section_absent() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert!(!config.aws_s3.enabled);
        assert!(config.aws_s3.bucket_allowlist.is_empty());
        assert!(config.aws_s3.assume_role_arn.is_none());
        assert_eq!(config.aws_s3.max_get_bytes, 100 * 1024 * 1024);
        assert!(config.aws_s3.require_approval_buckets.is_empty());
        assert!(!config.aws_s3.is_ready());
    }

    #[test]
    fn aws_s3_round_trip_from_toml() {
        let toml = r#"
[aws_s3]
enabled = true
bucket_allowlist = ["a", "b"]
assume_role_arn = "arn:aws:iam::123:role/r"
max_get_bytes = 1048576
require_approval_buckets = ["a"]
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert!(config.aws_s3.is_ready());
        assert_eq!(config.aws_s3.bucket_allowlist, vec!["a", "b"]);
        assert_eq!(
            config.aws_s3.assume_role_arn.as_deref(),
            Some("arn:aws:iam::123:role/r")
        );
        assert_eq!(config.aws_s3.max_get_bytes, 1_048_576);
        assert_eq!(config.aws_s3.require_approval_buckets, vec!["a"]);
    }

    #[test]
    fn aws_infra_defaults_when_section_absent() {
        let config = WorkforceConfig::from_toml("").unwrap();
        assert!(!config.aws_infra.enabled);
        assert!(config.aws_infra.assume_role_arn.is_none());
        assert!(config.aws_infra.region.is_none());
        assert!(!config.aws_infra.is_ready());
    }

    #[test]
    fn aws_infra_round_trip_from_toml() {
        let toml = r#"
[aws_infra]
enabled = true
assume_role_arn = "arn:aws:iam::123:role/atlas-infra-reader"
region = "eu-west-2"
"#;
        let config = WorkforceConfig::from_toml(toml).unwrap();
        assert!(config.aws_infra.is_ready());
        assert_eq!(
            config.aws_infra.assume_role_arn.as_deref(),
            Some("arn:aws:iam::123:role/atlas-infra-reader")
        );
        assert_eq!(config.aws_infra.region.as_deref(), Some("eu-west-2"));
    }

    #[test]
    fn aws_infra_not_ready_without_role_arn() {
        let config = WorkforceConfig::from_toml("[aws_infra]\nenabled = true\n").unwrap();
        assert!(config.aws_infra.enabled);
        assert!(!config.aws_infra.is_ready());
    }

    #[test]
    fn parse_csv_env_trims_and_drops_empties() {
        assert_eq!(parse_csv_env(""), Vec::<String>::new());
        assert_eq!(parse_csv_env("a"), vec!["a"]);
        assert_eq!(parse_csv_env(" a , ,b , "), vec!["a", "b"]);
        assert_eq!(parse_csv_env(",,"), Vec::<String>::new());
    }

    #[test]
    fn heartbeat_settings_tools_default_to_the_full_surface() {
        assert_eq!(toml::from_str::<HeartbeatSettings>("").unwrap().tools, None);
        assert_eq!(
            toml::from_str::<HeartbeatSettings>("tools = [\"github\"]")
                .unwrap()
                .tools,
            Some(vec!["github".to_string()])
        );
    }

    #[test]
    fn tool_loading_is_on_by_default_and_parses_per_agent() {
        let defaults = WorkforceConfig::default().tools.loading;
        assert_eq!(defaults.mode, ToolLoadingMode::On);
        assert_eq!(defaults.keep_turns, 10);
        assert_eq!(defaults.budget_for("any"), 0.10);
        let tools: ToolsConfig = toml::from_str(
            "[loading]\nmode = \"shadow\"\nagents = [\"alpha\"]\nalways = [\"github\"]\nkeep_turns = 4\nload_budget = 0.05\nbudgets = { alpha = 0.2 }\n",
        )
        .unwrap();
        assert_eq!(tools.loading.mode_for("alpha"), ToolLoadingMode::Shadow);
        assert_eq!(tools.loading.mode_for("beta"), ToolLoadingMode::Off);
        assert_eq!(tools.loading.always, vec!["github".to_string()]);
        assert_eq!(tools.loading.keep_turns, 4);
        assert_eq!(tools.loading.window_days, 14);
        assert_eq!(tools.loading.budget_for("alpha"), 0.2);
        assert_eq!(tools.loading.budget_for("beta"), 0.05);

        let everyone_but: ToolsConfig =
            toml::from_str("[loading]\nexcept = [\"gamma\"]\n").unwrap();
        assert_eq!(everyone_but.loading.mode_for("alpha"), ToolLoadingMode::On);
        assert_eq!(everyone_but.loading.mode_for("gamma"), ToolLoadingMode::Off);
    }

    #[test]
    fn programs_are_opt_in_by_default_and_on_only_for_the_agents_named() {
        let defaults = WorkforceConfig::default().tools.programs;
        assert_eq!(defaults.mode, ProgramsMode::OptIn);
        assert!(!defaults.enabled_for("alpha", None));
        assert!(
            defaults.enabled_for("alpha", Some(true)),
            "an agent opts itself in"
        );
        assert!(defaults.read_tools.is_empty());
        assert_eq!(defaults.sql_tool, "metabase.dataset_query_native");
        let off: ToolsConfig = toml::from_str("[programs]\nmode = \"off\"\n").unwrap();
        assert!(
            !off.programs.enabled_for("alpha", Some(true)),
            "off is off whatever the agent says"
        );

        let tools: ToolsConfig = toml::from_str(
            "[programs]\nmode = \"on\"\nagents = [\"alpha\", \"beta\"]\nexcept = [\"beta\"]\nread_tools = [\"metabase\"]\nmax_calls = 20\n",
        )
        .unwrap();
        assert!(tools.programs.enabled_for("alpha", None));
        assert!(!tools.programs.enabled_for("beta", None));
        assert!(!tools.programs.enabled_for("gamma", None));
        assert_eq!(tools.programs.read_tools, vec!["metabase".to_string()]);
        assert_eq!(tools.programs.max_calls, 20);
        assert_eq!(tools.programs.max_concurrency, 4);

        // An agent's own config.toml overrides the lists either way.
        assert!(!tools.programs.enabled_for("alpha", Some(false)));
        assert!(tools.programs.enabled_for("gamma", Some(true)));
        assert!(tools.programs.enabled_for("beta", Some(true)));
    }

    #[test]
    fn structural_editing_is_opt_in_and_the_install_keeps_the_language_servers_switch() {
        let install = WorkforceConfig::default().tools.refactor;
        assert_eq!(install.mode, ProgramsMode::OptIn);
        assert_eq!(install.for_agent(None), None);
        assert_eq!(
            (install.max_servers, install.max_memory_mb),
            (1, 2048),
            "an opted-in agent cannot take more than 2 GiB of a small host"
        );

        let agent: AgentWorkspaceConfig =
            toml::from_str("[tools.refactor]\nenabled = true\n").unwrap();
        let refactor = install.for_agent(agent.refactor()).expect("opted in");
        assert!(refactor.language_servers);
        assert_eq!(refactor.rust_analyzer, ["rust-analyzer"]);

        let tree_only: AgentWorkspaceConfig =
            toml::from_str("[tools.refactor]\nenabled = true\nlanguage_servers = false\n").unwrap();
        assert!(
            !install
                .for_agent(tree_only.refactor())
                .unwrap()
                .language_servers
        );

        let no_servers: ToolsConfig =
            toml::from_str("[refactor]\nlanguage_servers = false\nmax_servers = 1\n").unwrap();
        let wants_servers: AgentWorkspaceConfig =
            toml::from_str("[tools.refactor]\nenabled = true\nlanguage_servers = true\n").unwrap();
        let refactor = no_servers
            .refactor
            .for_agent(wants_servers.refactor())
            .unwrap();
        assert!(!refactor.language_servers, "an agent cannot turn them on");
        assert_eq!(refactor.max_servers, 1);

        let off: ToolsConfig = toml::from_str("[refactor]\nmode = \"off\"\n").unwrap();
        assert_eq!(off.refactor.for_agent(agent.refactor()), None);
    }

    #[test]
    fn the_compile_gate_is_opt_in_and_the_install_is_its_kill_switch() {
        let install = WorkforceConfig::default().tools.compile_gate;
        assert_eq!(install.mode, ProgramsMode::OptIn);
        assert_eq!(install.for_agent(None), None);

        let agent: AgentWorkspaceConfig = toml::from_str(
            "[tools.compile_gate]\nenabled = true\nmax_fix_rounds = 4\n\n[[tools.compile_gate.checks]]\nrepo = \"acme/api\"\ncommand = [\"cargo\", \"check\"]\n",
        )
        .unwrap();
        let gate = install
            .for_agent(agent.compile_gate())
            .expect("the agent opted in");
        assert_eq!(gate.max_fix_rounds, 4);
        assert_eq!(gate.timeout_secs, install.timeout_secs);
        assert_eq!(
            gate.checks,
            vec![DeclaredCheck {
                repo: "acme/api".into(),
                path: String::new(),
                command: vec!["cargo".into(), "check".into()],
            }]
        );

        let off: ToolsConfig = toml::from_str("[compile_gate]\nmode = \"off\"\n").unwrap();
        assert_eq!(off.compile_gate.for_agent(agent.compile_gate()), None);

        let on: ToolsConfig = toml::from_str("[compile_gate]\nmode = \"on\"\n").unwrap();
        assert!(on.compile_gate.for_agent(None).is_some());
        let declined = AgentCompileGateConfig {
            enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(on.compile_gate.for_agent(Some(&declined)), None);
    }

    #[test]
    fn research_is_not_adversarial_unless_the_agent_or_the_call_asks() {
        let install = WorkforceConfig::default().research;
        let policy = install.for_agent(None);
        assert!(
            !policy.adversarial(None),
            "off by default: it doubles the cost"
        );
        assert!(policy.adversarial(Some(true)), "a call may ask for it");
        assert_eq!(policy.low_confidence_below, DEFAULT_LOW_CONFIDENCE_BELOW);

        let workspace: AgentWorkspaceConfig =
            toml::from_str("[research]\nadversarial = true\nlow_confidence_below = 250\n").unwrap();
        let mine = install.for_agent(workspace.research.as_ref());
        assert!(mine.adversarial(None), "the agent's default applies");
        assert!(
            !mine.adversarial(Some(false)),
            "and a call may still decline"
        );
        assert_eq!(mine.low_confidence_below, 100, "clamped to the 0–100 scale");

        let killed: WorkforceConfig =
            toml::from_str("[research]\nallow_adversarial = false\n").unwrap();
        let refused = killed.research.for_agent(workspace.research.as_ref());
        assert!(!refused.adversarial(None) && !refused.adversarial(Some(true)));
    }

    #[test]
    fn recurring_jobs_may_pin_a_session_unless_the_install_or_the_agent_says_otherwise() {
        let install = WorkforceConfig::default().scheduler;
        assert!(install.for_agent(None).pinned_sessions);
        let own: AgentWorkspaceConfig =
            toml::from_str("[scheduler]\npinned_sessions = false\n").unwrap();
        assert!(!install.for_agent(own.scheduler.as_ref()).pinned_sessions);
        let silent: AgentWorkspaceConfig = toml::from_str("[scheduler]\n").unwrap();
        assert!(install.for_agent(silent.scheduler.as_ref()).pinned_sessions);

        let killed: WorkforceConfig =
            toml::from_str("[scheduler]\npinned_sessions = false\n").unwrap();
        let wants: AgentWorkspaceConfig =
            toml::from_str("[scheduler]\npinned_sessions = true\n").unwrap();
        assert!(
            !killed
                .scheduler
                .for_agent(wants.scheduler.as_ref())
                .pinned_sessions
        );
    }

    #[test]
    fn decisions_are_recorded_unless_the_install_or_the_agent_says_otherwise() {
        let install = WorkforceConfig::default().decision.records;
        assert_eq!((install.enabled, install.retention_days), (true, 90));
        assert!(install.for_agent(None).enabled);
        let own: AgentWorkspaceConfig =
            toml::from_str("[decision.records]\nenabled = false\n").unwrap();
        assert!(!install.for_agent(own.decision.as_ref()).enabled);
        let silent: AgentWorkspaceConfig = toml::from_str("[decision]\n").unwrap();
        assert!(install.for_agent(silent.decision.as_ref()).enabled);

        let killed: WorkforceConfig =
            toml::from_str("[decision.records]\nenabled = false\nretention_days = 7\n").unwrap();
        let wants: AgentWorkspaceConfig =
            toml::from_str("[decision.records]\nenabled = true\n").unwrap();
        let merged = killed.decision.records.for_agent(wants.decision.as_ref());
        assert_eq!((merged.enabled, merged.retention_days), (false, 7));

        let before: WorkforceConfig =
            toml::from_str("[decision]\nenabled = true\nprimary = \"kev\"\n").unwrap();
        assert_eq!(before.decision.records, DecisionRecordsConfig::default());
    }

    #[test]
    fn a_handoff_is_verified_only_when_the_agent_or_the_handoff_asks() {
        let install = WorkforceConfig::default().handoff;
        let policy = install.for_agent(None);
        assert!(!policy.verify(None), "off by default: it costs a call");
        assert!(policy.verify(Some(true)), "a step or call may ask for it");
        assert_eq!(policy.corrections(None), DEFAULT_HANDOFF_CORRECTIONS);
        assert_eq!(policy.model, None);

        let workspace: AgentWorkspaceConfig =
            toml::from_str("[handoff]\nverify = true\nmax_corrections = 9\nmodel = \"cheap\"\n")
                .unwrap();
        let mine = install.for_agent(workspace.handoff.as_ref());
        assert!(mine.verify(None), "the agent's default applies");
        assert!(!mine.verify(Some(false)), "and a hand-over may decline");
        assert_eq!(
            mine.corrections(None),
            MAX_HANDOFF_CORRECTIONS,
            "the agent's rounds are capped"
        );
        assert_eq!(mine.corrections(Some(0)), 0, "a hand-over may ask for none");
        assert_eq!(mine.corrections(Some(50)), MAX_HANDOFF_CORRECTIONS);
        assert_eq!(mine.model.as_deref(), Some("cheap"));

        let killed: WorkforceConfig = toml::from_str("[handoff]\nallow_verify = false\n").unwrap();
        let refused = killed.handoff.for_agent(workspace.handoff.as_ref());
        assert!(!refused.verify(None) && !refused.verify(Some(true)));
    }

    #[test]
    fn opt_in_programs_run_only_where_the_agent_config_turns_them_on() {
        let tools: ToolsConfig =
            toml::from_str("[programs]\nmode = \"opt_in\"\nagents = [\"alpha\"]\n").unwrap();
        assert_eq!(tools.programs.mode, ProgramsMode::OptIn);
        assert!(
            !tools.programs.enabled_for("alpha", None),
            "the install list does not opt an agent in"
        );
        assert!(!tools.programs.enabled_for("beta", Some(false)));
        assert!(tools.programs.enabled_for("beta", Some(true)));

        let workspace: AgentWorkspaceConfig =
            toml::from_str("[tools.programs]\nenabled = true\n").unwrap();
        assert_eq!(workspace.programs().and_then(|p| p.enabled), Some(true));
        assert!(AgentWorkspaceConfig::default().programs().is_none());
        let other: AgentWorkspaceConfig = toml::from_str("[tools]\n").unwrap();
        assert!(other.programs().is_none());
    }

    #[test]
    fn an_agents_own_programs_config_overrides_the_install_field_by_field() {
        let install: ToolsConfig = toml::from_str(
            "[programs]\nread_tools = [\"metabase\"]\nmax_calls = 20\ntimeout_secs = 90\n",
        )
        .unwrap();
        let install = install.programs;
        assert!(
            install.for_agent("alpha", None).is_none(),
            "opt_in: nothing without the agent"
        );

        let workspace: AgentWorkspaceConfig = toml::from_str(
            "[tools.programs]\nenabled = true\nread_tools = [\"repo\", \"metabase.search\"]\nmax_calls = 5\n",
        )
        .unwrap();
        let mine = install
            .for_agent("alpha", workspace.programs())
            .expect("opted in");
        assert_eq!(mine.read_tools, ["repo", "metabase.search"]);
        assert_eq!(mine.max_calls, 5);
        assert_eq!(mine.timeout_secs, 90, "unset fields keep the install's");
        assert_eq!(mine.sql_tool, "metabase.dataset_query_native");

        assert!(mine.hint, "the request hint defaults on");
        assert_eq!(mine.nudge_after, 3);
        let quiet: AgentWorkspaceConfig =
            toml::from_str("[tools.programs]\nenabled = true\nhint = false\nnudge_after = 0\n")
                .unwrap();
        let quiet = install
            .for_agent("alpha", quiet.programs())
            .expect("opted in");
        assert!(!quiet.hint);
        assert_eq!(quiet.nudge_after, 0);

        let off: AgentWorkspaceConfig =
            toml::from_str("[tools.programs]\nenabled = false\nmax_calls = 5\n").unwrap();
        assert!(install.for_agent("alpha", off.programs()).is_none());
    }

    #[test]
    fn video_frames_are_on_by_default_and_the_install_is_the_kill_switch() {
        let install = WorkforceConfig::default().attachments.video;
        let mine = install.for_agent(None).expect("on by default");
        assert_eq!(mine, install);

        let agent_off: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\nenabled = false\n").unwrap();
        assert!(install.for_agent(agent_off.video_frames()).is_none());

        let killed: WorkforceConfig =
            toml::from_str("[attachments.video]\nenabled = false\n").unwrap();
        let agent_on: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\nenabled = true\n").unwrap();
        assert!(
            killed
                .attachments
                .video
                .for_agent(agent_on.video_frames())
                .is_none(),
            "an agent cannot turn frames on past the install"
        );
        assert!(AgentWorkspaceConfig::default().video_frames().is_none());
    }

    #[test]
    fn an_agents_video_frames_config_overrides_field_by_field_within_ceilings() {
        let install: WorkforceConfig =
            toml::from_str("[attachments.video]\nmax_frames = 6\nmax_minutes = 3\n").unwrap();
        let install = install.attachments.video;

        let workspace: AgentWorkspaceConfig = toml::from_str(
            "[attachments.video]\nmax_frames = 10\njpeg_quality = 60\nscene_threshold = 0\n",
        )
        .unwrap();
        let mine = install.for_agent(workspace.video_frames()).expect("on");
        assert_eq!(mine.max_frames, 10);
        assert_eq!(mine.jpeg_quality, 60);
        assert_eq!(mine.scene_threshold, 0.0);
        assert_eq!(mine.max_minutes, 3, "unset fields keep the install's");
        assert_eq!(mine.max_long_edge, 1024);
        assert_eq!(mine.detail_long_edge, 1568);

        let greedy: AgentWorkspaceConfig = toml::from_str(
            "[attachments.video]\nmax_frames = 500\nmax_long_edge = 8000\njpeg_quality = 0\n\
             min_interval_secs = -3\nscene_threshold = 7\n",
        )
        .unwrap();
        let clamped = install.for_agent(greedy.video_frames()).expect("on");
        assert_eq!(clamped.max_frames, VideoFramesConfig::FRAMES_CEILING);
        assert_eq!(clamped.max_long_edge, VideoFramesConfig::LONG_EDGE_CEILING);
        assert_eq!(
            clamped.detail_long_edge,
            VideoFramesConfig::LONG_EDGE_CEILING
        );
        assert_eq!(clamped.jpeg_quality, 1);
        assert_eq!(clamped.min_interval_secs, 0.0);
        assert_eq!(clamped.scene_threshold, 1.0);

        let zero: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\nmax_frames = 0\nmax_long_edge = 1\n").unwrap();
        let floored = install.for_agent(zero.video_frames()).expect("on");
        assert_eq!(floored.max_frames, 1);
        assert_eq!(floored.max_long_edge, 128);

        let detail: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\nmax_long_edge = 800\ndetail_long_edge = 1200\n")
                .unwrap();
        let detailed = install.for_agent(detail.video_frames()).expect("on");
        assert_eq!(detailed.detail_long_edge, 1200);
        let shrunk: AgentWorkspaceConfig =
            toml::from_str("[attachments.video]\ndetail_long_edge = 300\n").unwrap();
        assert_eq!(
            install
                .for_agent(shrunk.video_frames())
                .expect("on")
                .detail_long_edge,
            1024,
            "a detailed frame is never smaller than a sampled one"
        );
    }

    #[test]
    fn tool_argument_validation_defaults_to_shadow_and_parses_every_mode() {
        assert_eq!(
            WorkforceConfig::default().tools.validation.mode,
            ArgumentValidation::Shadow
        );
        for (text, mode) in [
            ("off", ArgumentValidation::Off),
            ("shadow", ArgumentValidation::Shadow),
            ("enforce", ArgumentValidation::Enforce),
        ] {
            let tools: ToolsConfig =
                toml::from_str(&format!("[validation]\nmode = \"{text}\"\n")).unwrap();
            assert_eq!(tools.validation.mode, mode);
        }
    }

    #[test]
    fn decide_first_is_off_until_the_install_allows_it_and_an_agent_only_holds_it_back() {
        let stock = DecisionUses::default().decide_first;
        assert_eq!(stock.mode, DecisionUse::Off);
        assert_eq!(stock.min_probability.get(), 0.9);
        let before: DecisionConfig = toml::from_str("enabled = true\nprimary = \"kev\"\n").unwrap();
        assert_eq!(before.uses.decide_first, stock);

        let install = |mode: &str| -> DecideFirstUse {
            toml::from_str(&format!("mode = \"{mode}\"\nbudget_ms = 800\n")).unwrap()
        };
        let agent = |mode: &str| -> AgentWorkspaceConfig {
            toml::from_str(&format!(
                "[decision.uses.decide_first]\nmode = \"{mode}\"\n"
            ))
            .unwrap()
        };
        let silent: AgentWorkspaceConfig = toml::from_str("[decision.records]\n").unwrap();
        for (allowed, own, effective) in [
            ("on", None, DecisionUse::On),
            ("on", Some("shadow"), DecisionUse::Shadow),
            ("on", Some("off"), DecisionUse::Off),
            ("shadow", Some("on"), DecisionUse::Shadow),
            ("off", Some("on"), DecisionUse::Off),
            ("shadow", None, DecisionUse::Shadow),
        ] {
            let own = own.map_or(silent.clone(), agent);
            assert_eq!(
                install(allowed).for_agent(own.decision.as_ref()),
                effective,
                "{allowed} under {:?}",
                own.decision
            );
        }
        assert!(toml::from_str::<DecideFirstUse>("min_probability = 0.0\n").is_err());
    }

    #[test]
    fn decision_scope_gate_and_scope_classifier_parse_and_floors_are_bounded() {
        let uses: DecisionUses =
            toml::from_str("[scope_gate]\nmode = \"shadow\"\nmin_probability = 0.6\n").unwrap();
        assert_eq!(uses.scope_gate.mode, DecisionUse::Shadow);
        assert_eq!(uses.scope_gate.min_probability.get(), 0.6);
        assert_eq!(DecisionUses::default().scope_gate, ScopeGateUse::default());

        let scope: ScopeFilterConfig = toml::from_str(
            "persona = \"triage\"\nclassifier = \"decision\"\nmin_probability = 0.55\n",
        )
        .unwrap();
        assert_eq!(scope.classifier, Some(ScopeClassifier::Decision));
        assert_eq!(scope.min_probability.map(Probability::get), Some(0.55));
        for bad in ["min_probability = 0.0", "min_probability = 1.5"] {
            let err = toml::from_str::<ScopeFilterConfig>(bad)
                .unwrap_err()
                .to_string();
            assert!(err.contains("more than 0 and at most 1"), "{bad}: {err}");
        }
        assert!(
            toml::from_str::<BranchFallbackConfig>("min_probability = -1").is_err(),
            "the branch fallback floor is bounded the same way"
        );
    }

    #[test]
    fn decision_limits_parse_on_every_use_and_configs_without_them_still_do() {
        let legacy: DecisionConfig = toml::from_str(
            "enabled = true\nprimary = \"kev\"\n\
             [[backends]]\nname = \"kev\"\nurl = \"http://kev:8009\"\n\
             [branch_fallback]\nshadow = false\n\
             [uses]\ndecision_steps = true\ntool = false\n\
             [uses.scope_gate]\nmode = \"on\"\n",
        )
        .unwrap();
        assert_eq!(legacy.backends[0].max_concurrent, 4);
        assert_eq!(legacy.branch_fallback.limits, DecisionLimits::default());
        assert!(legacy.uses.decision_steps.enabled() && !legacy.uses.tool.enabled());
        assert_eq!(legacy.uses.scope_gate.limits, DecisionLimits::default());
        assert_eq!(legacy.uses.routing, RoutingUse::default());
        assert_eq!(legacy.uses.routing.mode, DecisionUse::Off);
        let defaults = DecisionUses::default();
        assert!(defaults.decision_steps.enabled() && defaults.tool.enabled());

        let limited: DecisionConfig = toml::from_str(
            "[[backends]]\nname = \"kev\"\nurl = \"http://kev:8009\"\nmax_concurrent = 4\n\
             [branch_fallback]\nbudget_ms = 4000\nstate_chars = 3000\n\
             [uses]\ndecision_steps = { budget_ms = 20000 }\n\
             tool = { enabled = false, state_chars = 800 }\n\
             [uses.scope_gate]\nmode = \"shadow\"\nbudget_ms = 900\n\
             [uses.routing]\nmode = \"shadow\"\nmin_probability = 0.6\nbudget_ms = 250\n",
        )
        .unwrap();
        let ms = |limits: DecisionLimits| limits.budget_ms.map(std::num::NonZeroU64::get);
        let chars = |limits: DecisionLimits| limits.state_chars.map(StateChars::get);
        assert_eq!(limited.backends[0].max_concurrent, 4);
        assert_eq!(ms(limited.branch_fallback.limits), Some(4000));
        assert_eq!(chars(limited.branch_fallback.limits), Some(3000));
        assert!(limited.branch_fallback.shadow);
        assert!(limited.uses.decision_steps.enabled());
        assert_eq!(ms(limited.uses.decision_steps.limits()), Some(20000));
        assert!(!limited.uses.tool.enabled());
        assert_eq!(chars(limited.uses.tool.limits()), Some(800));
        assert_eq!(limited.uses.scope_gate.mode, DecisionUse::Shadow);
        assert_eq!(ms(limited.uses.scope_gate.limits), Some(900));
        assert_eq!(chars(limited.uses.scope_gate.limits), None);
        assert_eq!(limited.uses.routing.mode, DecisionUse::Shadow);
        assert_eq!(limited.uses.routing.min_probability.get(), 0.6);
        assert_eq!(ms(limited.uses.routing.limits), Some(250));

        let again: DecisionConfig = toml::from_str(&toml::to_string(&limited).unwrap()).unwrap();
        assert_eq!(again.uses.tool, limited.uses.tool);
        assert_eq!(again.uses.routing, limited.uses.routing);
        assert_eq!(again.branch_fallback.limits, limited.branch_fallback.limits);

        for bad in ["budget_ms = 0", "state_chars = 50", "min_probability = 0"] {
            assert!(toml::from_str::<RoutingUse>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_bad_use_switch_says_what_is_wrong_with_it() {
        let error = |toml: &str| {
            toml::from_str::<DecisionUses>(toml)
                .unwrap_err()
                .to_string()
        };
        for (bad, says) in [
            (
                "tool = { state_chars = 50 }",
                "state_chars must be at least 200, got 50",
            ),
            ("tool = { budget_ms = 0 }", "nonzero"),
            (
                "decision_steps = { budgt_ms = 500 }",
                "unknown field `budgt_ms`",
            ),
            (
                "decision_steps = { enabled = \"yes\" }",
                "expected a boolean",
            ),
            (
                "tool = \"on\"",
                "expected true, false, or a table of `enabled`, `budget_ms` and `state_chars`",
            ),
            ("decision_steps = 1", "expected true, false, or a table"),
        ] {
            let error = error(bad);
            assert!(error.contains(says), "{bad}: {error}");
            assert!(!error.contains("untagged"), "{bad}: {error}");
        }
        let json: DecisionUses = serde_json::from_value(serde_json::json!({
            "decision_steps": false, "tool": { "budget_ms": 2000 }
        }))
        .unwrap();
        assert!(!json.decision_steps.enabled() && json.tool.enabled());
        let ms = json.tool.limits().budget_ms.map(std::num::NonZeroU64::get);
        assert_eq!(ms, Some(2000));
    }

    #[test]
    fn login_policy_parses_under_llm_agents_and_workspace() {
        let config: WorkforceConfig = toml::from_str(
            r#"
            [llm.logins]
            tiers = ["agent_shared"]

            [subscription]
            chatgpt_direct = true

            [[agents]]
            id = "allan"
            [agents.llm.logins]
            tiers = ["global_user", "agent_shared"]
            shared_serves = "agent_users"

            [[agents]]
            id = "sarah"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        assert!(config.subscription.chatgpt_direct);

        let allan = config.agents[0].effective_llm(&config.llm);
        assert_eq!(
            allan.logins,
            LoginPolicy {
                tiers: vec![CredentialTier::GlobalUser, CredentialTier::AgentShared],
                shared_serves: SharedServes::AgentUsers,
            }
        );
        let sarah = config.agents[1].effective_llm(&config.llm);
        assert_eq!(sarah.logins.tiers, [CredentialTier::AgentShared]);
        assert_eq!(sarah.logins.shared_serves, SharedServes::Anyone);

        let workspace: AgentWorkspaceConfig = toml::from_str(
            r#"
            [llm.logins]
            shared_serves = "anyone"
            "#,
        )
        .unwrap();
        assert_eq!(
            workspace.merge_llm(&allan).logins,
            LoginPolicy {
                tiers: LoginPolicy::default().tiers,
                shared_serves: SharedServes::Anyone,
            }
        );
        assert_eq!(workspace.merge_llm(&sarah).logins.tiers.len(), 4);
    }

    #[test]
    fn completion_pool_defaults_to_four_with_or_without_the_section() {
        let absent: WorkforceConfig = toml::from_str("").unwrap();
        assert_eq!(absent.subscription.max_concurrent_completions, 4);
        assert_eq!(absent.subscription.max_concurrent_cli_turns, 4);
        let turns: WorkforceConfig =
            toml::from_str("[subscription]\nmax_concurrent_cli_turns = 2\n").unwrap();
        assert_eq!(turns.subscription.max_concurrent_cli_turns, 2);
        assert_eq!(turns.subscription.max_concurrent_completions, 4);
        let keyless: WorkforceConfig =
            toml::from_str("[subscription]\nchatgpt_direct = true\n").unwrap();
        assert_eq!(keyless.subscription.max_concurrent_completions, 4);
        let unbounded: WorkforceConfig =
            toml::from_str("[subscription]\nmax_concurrent_completions = 0\n").unwrap();
        assert_eq!(unbounded.subscription.max_concurrent_completions, 0);
        assert!(toml::from_str::<WorkforceConfig>(
            "[subscription]\nmax_concurrent_completions = -1\n"
        )
        .is_err());
    }

    #[test]
    fn default_logins_try_every_tier_and_serve_anyone_a_policy_narrows() {
        assert_eq!(
            LoginPolicy::default(),
            LoginPolicy {
                tiers: vec![
                    CredentialTier::AgentUser,
                    CredentialTier::GlobalUser,
                    CredentialTier::AgentShared,
                    CredentialTier::SystemShared,
                ],
                shared_serves: SharedServes::Anyone,
            },
            "where a login is stored alone decides whom it serves"
        );
        let config: WorkforceConfig = toml::from_str(
            r#"
            [[agents]]
            id = "allan"
            [agents.llm.logins]
            tiers = ["agent_user", "global_user", "agent_shared"]
            shared_serves = "owner"
            "#,
        )
        .unwrap();
        config.validate().unwrap();
        let allan = config.agents[0].effective_llm(&config.llm).logins;
        assert_eq!(
            allan.tiers,
            [
                CredentialTier::AgentUser,
                CredentialTier::GlobalUser,
                CredentialTier::AgentShared
            ],
            "an explicit policy may leave the install's login out"
        );
        assert_eq!(allan.shared_serves, SharedServes::Owner);
        assert_eq!(config.llm.logins, LoginPolicy::default());
    }

    #[test]
    fn login_policy_refuses_empty_and_repeated_tiers() {
        let policy = |tiers: Vec<CredentialTier>| LoginPolicy {
            tiers,
            shared_serves: SharedServes::Owner,
        };
        assert!(policy(vec![]).validate().is_err());
        let err = policy(vec![
            CredentialTier::AgentUser,
            CredentialTier::AgentShared,
            CredentialTier::AgentUser,
        ])
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("agent_user"), "{err}");
        assert!(LoginPolicy::default().validate().is_ok());
        assert!(toml::from_str::<LoginPolicy>(r#"tiers = ["owner"]"#).is_err());
    }

    #[test]
    fn workspace_parse_applies_the_routing_migration_load_does() {
        let body = "[routing]\nenable_adaptive_routing = true\ncoding_switch_round = 4\n";
        let parsed = AgentWorkspaceConfig::parse(body).unwrap();
        let routing = parsed.llm.as_ref().and_then(|llm| llm.routing.as_ref());
        assert_eq!(
            routing.and_then(|r| r.enable_adaptive_routing),
            Some(true),
            "a body is checked as the boot load will read it"
        );
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), body).unwrap();
        let loaded = AgentWorkspaceConfig::load(dir.path()).unwrap().unwrap();
        assert_eq!(
            loaded
                .llm
                .and_then(|llm| llm.routing)
                .and_then(|r| r.coding_switch_round),
            routing.and_then(|r| r.coding_switch_round)
        );
        assert!(AgentWorkspaceConfig::parse("[llm\n").is_err());
    }

    struct RefusesModel(&'static str);

    impl AdmitWorkspace for RefusesModel {
        fn admit(&self, cfg: &AgentWorkspaceConfig) -> eyre::Result<()> {
            match cfg
                .llm
                .as_ref()
                .and_then(|llm| llm.default_model.as_deref())
            {
                Some(model) if model == self.0 => Err(eyre::eyre!("no")),
                _ => Ok(()),
            }
        }
    }

    #[test]
    fn update_saves_only_what_admission_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let set = |model: &'static str| {
            move |cfg: &mut AgentWorkspaceConfig| {
                cfg.llm.get_or_insert_with(Default::default).default_model = Some(model.into())
            }
        };
        let admission = RefusesModel("refused");
        AgentWorkspaceConfig::update(dir.path(), &admission, set("kept")).unwrap();
        assert!(AgentWorkspaceConfig::update(dir.path(), &admission, set("refused")).is_err());
        let model = AgentWorkspaceConfig::load(dir.path())
            .unwrap()
            .and_then(|cfg| cfg.llm)
            .and_then(|llm| llm.default_model);
        assert_eq!(model.as_deref(), Some("kept"));
    }

    #[test]
    fn know_how_source_agent_is_read_only_on_a_test_copy() {
        let install = KnowHowConfig::default();
        let own = |body: &str| AgentWorkspaceConfig::parse(body).unwrap();
        let copy =
            own("[evals]\ncopy = true\n[know_how]\nenabled = true\nsource_agent = \"analyst\"\n");
        let cfg = install.for_workspace(Some(&copy)).unwrap();
        assert_eq!(cfg.source("analyst-eval"), "analyst");
        assert!(cfg.read_only());
        for ignored in [
            "[know_how]\nenabled = true\nsource_agent = \"analyst\"\n",
            "[evals]\ncopy = false\n[know_how]\nenabled = true\nsource_agent = \"analyst\"\n",
            "[evals]\ncopy = true\n[know_how]\nenabled = true\nsource_agent = \" \"\n",
        ] {
            let cfg = install.for_workspace(Some(&own(ignored))).unwrap();
            assert_eq!(cfg.source("analyst-eval"), "analyst-eval", "{ignored}");
            assert!(!cfg.read_only(), "{ignored}");
        }
        assert_eq!(
            own("[know_how]\nsource_agent = \"analyst\"\n").know_how_source_set(),
            Some("analyst")
        );
        let from_install: WorkforceConfig =
            toml::from_str("[know_how]\nsource_agent = \"analyst\"\n").unwrap();
        assert_eq!(from_install.know_how.source_agent, None);
    }

    #[test]
    fn an_agent_may_leave_know_how_kinds_out_but_not_add_one_the_install_leaves_out() {
        let own = |body: &str| AgentWorkspaceConfig::parse(body).unwrap();
        let kinds = |install: &KnowHowConfig, body: &str| {
            install.for_workspace(Some(&own(body))).unwrap().kinds
        };
        let install = KnowHowConfig::default();
        assert_eq!(
            kinds(&install, "[know_how]\nenabled = true\n"),
            KnowHowKind::ALL
        );
        let shapes =
            "[know_how]\nenabled = true\nkinds = [\"query_shape\", \"schema_correction\"]\n";
        assert_eq!(
            kinds(&install, shapes),
            [KnowHowKind::QueryShape, KnowHowKind::SchemaCorrection]
        );
        assert!(kinds(&install, "[know_how]\nenabled = true\nkinds = []\n").is_empty());
        let no_code: WorkforceConfig = toml::from_str(
            "[know_how]\nkinds = [\"data_location\", \"query_shape\", \"schema_correction\"]\n",
        )
        .unwrap();
        assert_eq!(
            kinds(
                &no_code.know_how,
                "[know_how]\nenabled = true\nkinds = [\"code_location\", \"query_shape\"]\n"
            ),
            [KnowHowKind::QueryShape]
        );
        assert!(AgentWorkspaceConfig::parse("[know_how]\nkinds = [\"query_shapes\"]\n").is_err());
    }

    mod environments {
        use super::super::*;

        const EXAMPLE: &str = r#"
[environments]
enabled = true
default = "driftwerk"

[environments.defs.driftwerk]
description = "Driftwerk and Fabrikam tracking"
skills = { linear = ["driftwerk", "fabrikam-crm"], metabase = ["prod"] }
mcp = { harbor = "prod" }

[environments.defs.scratch]
draft = true
"#;

        fn refusal(body: &str) -> String {
            AgentWorkspaceConfig::parse(body).unwrap_err().to_string()
        }

        #[test]
        fn the_documented_example_parses() {
            let envs = AgentWorkspaceConfig::parse(EXAMPLE)
                .unwrap()
                .environments
                .unwrap();
            assert_eq!(envs.default.as_deref(), Some("driftwerk"));
            assert_eq!(
                envs.defs["driftwerk"].skills["linear"],
                vec!["driftwerk", "fabrikam-crm"]
            );
            assert_eq!(envs.defs["driftwerk"].mcp["harbor"], "prod");
            assert!(envs.defs["scratch"].draft);
            assert!(!envs.defs["driftwerk"].draft);
        }

        #[test]
        fn admission_refuses_a_malformed_table() {
            let empty = "[environments.defs.a]\nskills = { linear = [] }\n";
            assert!(refusal(empty).contains("to no instance"));
            let twice = "[environments.defs.a]\nskills = { linear = [\"x\", \"x\"] }\n";
            assert!(refusal(twice).contains("twice"));
            let dangling = "[environments]\ndefault = \"ghost\"\n";
            assert!(refusal(dangling).contains("not an environment"));
            let draft = "[environments]\ndefault = \"a\"\n[environments.defs.a]\ndraft = true\n";
            assert!(refusal(draft).contains("still a draft"));
            let reserved = "[environments.defs.none]\n";
            assert!(refusal(reserved).contains("cannot name one"));
            let typo = "[environments.defs.a]\nskils = { linear = [\"x\"] }\n";
            assert!(refusal(typo).contains("unknown field"));
        }

        #[test]
        fn the_install_switch_allows_unless_explicitly_off() {
            assert!(EnvironmentsConfig::default().allowed());
            assert!(EnvironmentsConfig {
                enabled: Some(true)
            }
            .allowed());
            assert!(!EnvironmentsConfig {
                enabled: Some(false)
            }
            .allowed());
        }
    }
}

#[cfg(test)]
mod shadow_config_tests;
