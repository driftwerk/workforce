use std::path::{Path, PathBuf};
use std::sync::Arc;
use workforce_hub::serve::Compose as _;

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use console::style;
use eyre::Result;
use workforce_agent::llm::failover::{FailoverConfig, FailoverProvider};
use workforce_agent::llm::provider::{
    create_provider_async, provider_env_key, provider_env_var_hint, LlmProvider, ProviderKind,
    ProviderParseError,
};
use workforce_agent::runner::{default_tools, AgentRunner, RunnerConfig, ToolSecrets};
use workforce_common::identity::AgentIdentity;
use workforce_core::types::AgentId;
use workforce_policy::engine::PolicyEngine;

mod decide;
mod docs;
mod evals;
mod local_agent;
mod shadow;
mod training;
mod tui;

// ─── Global allocator ────────────────────────────────────────────────────────
//
// Use jemalloc instead of the system (glibc) allocator. The hub makes many
// large, short-lived allocations per turn — assembled prompt contexts, MCP
// tool catalogues, tool outputs, compaction copies. glibc malloc parks freed
// memory in per-thread arenas and rarely returns it to the OS; worse, it sizes
// its arena pool from the host's core count (8 on the prod box) while the
// container is pinned to 1 vCPU, so RSS ratchets up under load and only resets
// on restart — eventually OOM-killing at the cgroup cap. jemalloc with
// background purge threads and ~1s decay (configured via `malloc_conf` below)
// hands dirty pages back to the OS promptly, flattening that curve.
//
// The `profiling` feature lets us flip `prof:true` through `MALLOC_CONF` to
// capture a heap profile without a rebuild if a genuine live leak ever needs
// pinning. Not supported on MSVC, where this falls back to the system allocator.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// Run page purging on background threads and return dirty/muzzy pages to the OS
// within ~1s of being freed. `unprefixed_malloc_on_supported_platforms` makes
// `malloc_conf` the symbol jemalloc reads at init (rather than a prefixed one).
#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:1000\0";

#[derive(Parser)]
#[command(name = "workforce")]
#[command(about = "AI Workforce Framework — orchestrate AI agents as a coordinated team")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// API key (or set ANTHROPIC_API_KEY / OPENROUTER_API_KEY env var).
    #[arg(long, global = true, hide = true)]
    api_key: Option<String>,

    /// LLM provider: anthropic, openrouter, grok, gemini or ollama.
    #[arg(long, global = true, default_value = "anthropic")]
    provider: String,

    /// Working directory for file operations.
    #[arg(long, default_value = ".", global = true)]
    workdir: PathBuf,

    /// Policy file path.
    #[arg(long, global = true)]
    policy: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a one-shot task with an agent.
    Run {
        /// The task description.
        task: String,
        /// Agent ID.
        #[arg(short, long, default_value = "default")]
        agent: String,
        /// LLM model to use.
        #[arg(short, long)]
        /// Maximum tool execution rounds.
        #[arg(long, default_value = "20")]
        max_rounds: u32,
    },
    /// Interactive chat session with an agent.
    Chat {
        /// Agent ID.
        #[arg(short, long, default_value = "default")]
        agent: String,
        /// LLM model to use.
        #[arg(short, long)]
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
    /// Show system status.
    Status,
    /// Initialise configuration files.
    Init,
    /// Start the agent daemon (long-running service).
    Serve {
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Serve an agent's tools over MCP (Model Context Protocol), for an
    /// operator pointing an MCP client at them.
    ///
    /// Loads the named agent's tool registry and serves JSON-RPC requests over
    /// stdin/stdout until stdin closes, or over HTTP with `--listen`. Logs to
    /// stderr. The hub never spawns it: its own rounds are served in process.
    #[command(name = "mcp-serve")]
    McpServe {
        /// Agent ID to serve tools for.
        #[arg(long)]
        agent_id: String,
        /// Comma-separated tool patterns to expose (e.g. `knowledge.*,linear.*`).
        /// If omitted, exposes all tools in the agent's registry.
        #[arg(long, value_delimiter = ',')]
        exposed_tools: Vec<String>,
        /// Comma-separated skills to load from the agent's vault at startup
        /// (e.g. `linear,github`). Loads `skill_loader::load_skill_from_vault`
        /// for each so the corresponding tools appear in `tools/list`.
        #[arg(long, value_delimiter = ',')]
        preload_skills: Vec<String>,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Canonical workforce identity UUID, scoped via `CURRENT_USER_ID`
        /// for the lifetime of the subprocess so per-user vault paths
        /// (`users/<id>/...`) resolve through `LayeredUserVault`.
        #[arg(long)]
        user_id: Option<String>,
        /// Serve over HTTP on this address instead of stdin/stdout. Requires
        /// `--auth-token-file`.
        #[arg(long)]
        listen: Option<String>,
        /// File holding the bearer token every HTTP request must present. Read
        /// from a file rather than an argument so it never appears in `ps`.
        #[arg(long)]
        auth_token_file: Option<PathBuf>,
    },
    /// Run the privilege-separated command runner daemon (audit F7).
    ///
    /// Launched by the image init as the unprivileged `agent` user. Listens on
    /// a unix socket, authenticates each peer via `SO_PEERCRED` (accepting only
    /// `--accept-uid`), and executes one command per connection. The hub
    /// dispatches `exec.run` / `code.*` here so agent commands run as `agent`,
    /// which cannot read the `hub` user's vault, env, or `/proc`.
    #[command(name = "exec-runner")]
    ExecRunner {
        /// Unix socket path to bind (e.g. `/run/workforce/exec.sock`).
        #[arg(long)]
        socket: PathBuf,
        /// The uid (the `hub` user) whose connections are accepted; all others
        /// are rejected.
        #[arg(long)]
        accept_uid: u32,
    },
    /// Keep Desk's terminals across hub restarts: a small long-lived process
    /// that owns their PTYs and serves them to the hub over a unix socket.
    ///
    /// Started by Desk's helper before the hub and left running when the hub
    /// restarts. Reads its token from `WORKFORCE_PTY_HOLDER_TOKEN` (never an
    /// argument, so it stays out of `ps`). Exits at once if a holder already
    /// serves the socket; SIGTERM ends it and its terminals.
    #[command(name = "pty-holder")]
    PtyHolder {
        /// Unix socket path (its folder is made owner-only).
        #[arg(long)]
        socket: PathBuf,
    },
    /// Run a task using a team from the config.
    Team {
        /// The task description.
        task: String,
        /// Team ID (from workforce.toml).
        #[arg(short = 't', long)]
        team: Option<String>,
        /// Workflow template (e.g. "product-brainstorm", "dev-ticket").
        #[arg(short = 'w', long)]
        workflow: Option<String>,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// List and inspect configured teams.
    Teams {
        #[command(subcommand)]
        command: TeamsCommand,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Node management commands.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Skill management commands.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Plugin management commands.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// List available agents on a remote gateway.
    Agents {
        /// Gateway URL (e.g. ws://100.64.0.10:8091).
        #[arg(long)]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long)]
        token: Option<String>,
    },
    /// Session management commands.
    Sessions {
        /// Show session history
        #[arg(long)]
        history: bool,
        /// Redact secrets at rest across stored sessions
        #[arg(long)]
        redact: bool,
        /// With --redact, report what would change without writing
        #[arg(long)]
        dry_run: bool,
        /// Filter by agent name
        #[arg(long)]
        agent: Option<String>,
        /// Show sessions from last N days
        #[arg(long)]
        days: Option<u32>,
        /// Maximum number of sessions to show
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// List all available tools and their descriptions.
    Tools,
    /// Scheduler (cron) management commands.
    Cron {
        #[command(subcommand)]
        command: CronCommand,
    },
    /// Secrets vault management.
    Vault {
        #[command(subcommand)]
        command: VaultCommand,
    },
    /// Pairing management (channel access, device tokens, node approval).
    Pair {
        #[command(subcommand)]
        command: PairCommand,
        /// Data directory (for local-mode pairing database).
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    /// Lesson-corpus operations (extraction backfill, etc.).
    Lessons {
        #[command(subcommand)]
        command: LessonsCommand,
    },
    /// Run security audit checks against configuration and environment.
    Audit {
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Data directory to check.
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    /// Review and manage policy proposals from agents.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
        /// Data directory (for the proposals database).
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    /// Manage role templates.
    Role {
        #[command(subcommand)]
        command: RoleCommand,
        /// Config file path (for data_dir).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Slack app scaffolding.
    Slack {
        #[command(subcommand)]
        command: SlackCommand,
        /// Config file path (for data_dir / vault).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Google Workspace OAuth login and account management.
    Google {
        #[command(subcommand)]
        command: GoogleCommand,
        /// Config file path (for vault access).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Manage local agent configurations.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Manage heartbeat checks for agents.
    Heartbeat {
        #[command(subcommand)]
        command: HeartbeatCommand,
        /// Config file path (for workspace resolution).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Connect as a local agent to a remote gateway.
    LocalAgent {
        #[command(subcommand)]
        command: LocalAgentCommand,
    },
    /// Run this host as a workforce worker: the execution host the hub
    /// dispatches scopes to over mutual TLS.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// TOML-defined workflow management.
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommand,
        /// Workflows directory.
        #[arg(long, default_value = ".driftwerk/workflows")]
        workflows_dir: PathBuf,
        /// Data directory (for instance database).
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    /// Manage interactive playbook definitions (list, validate, show, create).
    Playbook {
        #[command(subcommand)]
        command: PlaybookCommand,
        /// Playbooks directory.
        #[arg(long, default_value = ".driftwerk/playbooks")]
        playbooks_dir: PathBuf,
    },
    /// View agent activity logs and statistics.
    Activity {
        /// Agent ID to filter by (shows all agents if omitted).
        #[arg(short, long)]
        agent: Option<String>,
        /// Show activity from last duration (e.g. "1h", "30m", "7d").
        #[arg(long, conflicts_with_all = ["from", "to"])]
        last: Option<String>,
        /// Start time (ISO8601 or "YYYY-MM-DD HH:MM:SS").
        #[arg(long, requires = "to")]
        from: Option<String>,
        /// End time (ISO8601 or "YYYY-MM-DD HH:MM:SS").
        #[arg(long, requires = "from")]
        to: Option<String>,
        /// Show detailed parameters and results.
        #[arg(short, long)]
        verbose: bool,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
    /// Migrate to identity system: seed identities, assign agent ownership, migrate memory.
    Migrate {
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Identity database path.
        #[arg(long, default_value = "data/identities.db")]
        identity_db: PathBuf,
        /// Dry run — show what would be done without making changes.
        #[arg(long)]
        dry_run: bool,
    },
    /// Copy a SQLite telemetry.db into the configured Postgres backend.
    ///
    /// Reads `[postgres]` from the workforce config to choose the
    /// destination, runs the telemetry refinery migrations, and then
    /// batch-inserts every row from the source. Idempotent — re-running
    /// against a partially-migrated destination is safe (rows that
    /// already exist are skipped via `ON CONFLICT DO NOTHING`).
    ///
    /// Recommended workflow: stop the hub, run this command, set
    /// `[telemetry] backend = "postgres"`, restart the hub.
    MigrateTelemetry {
        /// Workforce config file (used for the Postgres connection).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// SQLite source `telemetry.db` to copy from. Defaults to
        /// `{hub.data_dir}/telemetry.db`.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite identities.db into the configured Postgres backend.
    /// Same idempotent semantics as `migrate-telemetry`. Recommended
    /// workflow: stop the hub, run this command, set
    /// `[identity] backend = "postgres"`, restart the hub.
    MigrateIdentity {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// SQLite source `identities.db`. Defaults to
        /// `{hub.data_dir}/identities.db`.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite pairing.db into the configured Postgres backend.
    /// Same idempotent semantics as `migrate-telemetry`. Recommended
    /// workflow: stop the hub, run this command, set
    /// `[pairing_store] backend = "postgres"`, restart the hub.
    MigratePairing {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// SQLite source `pairing.db`. Defaults to
        /// `{hub.data_dir}/pairing.db`.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite codegraph.db into the configured Postgres backend.
    /// Same idempotent semantics as `migrate-telemetry`. Recommended
    /// workflow: stop the hub, run this command, set
    /// `[codegraph] backend = "postgres"`, restart the hub.
    MigrateCodegraph {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// SQLite source `codegraph.db`. Defaults to
        /// `{hub.data_dir}/.workforce/knowledge/codegraph.db`.
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy per-agent JSONL sessions into the configured Postgres backend.
    /// Same idempotent semantics as `migrate-telemetry`. Recommended
    /// workflow: quiesce writes (stop the hub), run this command, set
    /// `[sessions] backend = "postgres"`, restart the hub. Note: each
    /// agent's whole workspace is loaded into memory while it is copied —
    /// size the migration container accordingly.
    MigrateSessions {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Copy per-agent scheduler `jobs.json` files into the configured
    /// Postgres backend. Idempotent, and never overwrites a row the live
    /// store has since updated. Recommended workflow: quiesce writes (stop
    /// the hub), run this command, set `[scheduler_store] backend =
    /// "postgres"`, restart the hub.
    MigrateScheduler {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Copy a SQLite mcp_store.db into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[mcp_store] backend = "postgres"`.
    MigrateMcpStore {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite projects.db into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[projects] backend = "postgres"`.
    MigrateProjects {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite permissions.db into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[permissions] backend = "postgres"`.
    MigratePermissions {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite training DB into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[training] backend = "postgres"`.
    MigrateTraining {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite self_learning.db into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[self_learning] backend = "postgres"`.
    MigrateSelfLearning {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Copy a SQLite approvals.db into the configured Postgres backend.
    /// Idempotent. Stop the hub (quiesce writes), then set `[approvals] backend = "postgres"`.
    MigrateApprovals {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Weaviate memory store operations (schema validation, rebuild).
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Shadow mode (Weaviate serving, vellev compared): status, the weekly
    /// report against the exit criteria, how to pause or resume, and
    /// promoting vellev to serve a shadowed store or rolling it back.
    Shadow {
        #[command(subcommand)]
        command: shadow::ShadowCommand,
    },
    /// Artifact knowledge-base operations.
    Artifacts {
        #[command(subcommand)]
        command: ArtifactsCommand,
    },
    /// Code knowledge operations.
    Code {
        #[command(subcommand)]
        command: CodeCommand,
    },
    /// Permission cascade operations — migrate legacy YAML rules into the
    /// permission store, with dry-run-by-default and a CSV report for
    /// `role`-scope rules that need manual mapping.
    Permissions {
        #[command(subcommand)]
        command: PermissionsCommand,
    },
    /// Doc-curator agent operations — draft end-user docs from source code.
    Docs {
        #[command(subcommand)]
        command: docs::DocsCommand,
    },
    /// Agent training — load reference corpora the agent retrieves
    /// automatically via semantic recall (DW-539).
    Training {
        #[command(subcommand)]
        command: training::TrainingCommand,
    },
    /// Decision-model operations: gather an install's recorded decisions,
    /// build training records from them with the hub's own state builders,
    /// and put them to the configured backends.
    #[command(hide = true)]
    Decide {
        #[command(subcommand)]
        command: decide::DecideCommand,
    },
    /// Run a golden set of questions against an agent on a running hub and
    /// score the answers
    Eval {
        #[command(subcommand)]
        command: evals::EvalCommand,
    },
    /// Export agents (config, memories, transcripts) to a portable `.zip` bundle.
    Export {
        /// Agent IDs to export.
        #[arg(long, value_delimiter = ',', required = true)]
        agents: Vec<String>,
        /// Output bundle path.
        #[arg(short, long)]
        out: PathBuf,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// This install's Slack workspace team id, recorded so a same-workspace
        /// import can reconcile identities by Slack id as well as email.
        #[arg(long)]
        slack_team_id: Option<String>,
    },
    /// Import an agent bundle (`.zip`) into this install, reconciling identities.
    Import {
        /// Bundle path.
        bundle: PathBuf,
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Print the merge plan and exit without writing anything.
        #[arg(long)]
        plan_only: bool,
        /// Rename a source agent on import: `--rename old=new` (repeatable).
        #[arg(long = "rename", value_parser = parse_key_value)]
        rename: Vec<(String, String)>,
        /// Map a source identity uuid to an existing one: `--map-identity src=dst`.
        #[arg(long = "map-identity", value_parser = parse_key_value)]
        map_identity: Vec<(String, String)>,
        /// Apply without the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// This install's Slack workspace team id (for same-workspace reconcile).
        #[arg(long)]
        slack_team_id: Option<String>,
    },
}

fn parse_key_value(raw: &str) -> Result<(String, String), String> {
    raw.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got '{raw}'"))
}

#[derive(Subcommand)]
enum PermissionsCommand {
    /// Translate the YAML `policy_path` rules into permission_rules.
    /// Defaults to dry-run; pass `--apply` to actually upsert rows.
    Migrate {
        /// Workforce config file (resolves policy_path + permission backend).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Actually write rows to the permission store. Without this
        /// flag the command only prints what would change.
        #[arg(long)]
        apply: bool,
        /// Where to write the role-scope review CSV. Defaults to
        /// `{hub.data_dir}/permissions-role-migration.csv`. Empty rules
        /// mean no file is written.
        #[arg(long)]
        csv_out: Option<PathBuf>,
    },
    /// Read the operator-edited role-scope CSV produced by `migrate`
    /// (with `team_id` populated) and materialise each row as a
    /// Team-scope `PermissionRule`. Defaults to dry-run.
    ///
    /// Expected columns: `name,role,team_id,tool_pattern,action,priority`.
    /// Rows with an empty `team_id` are skipped. The `name` and `role`
    /// columns are read for context only — output rows are keyed on
    /// `(team_id, tool_pattern)`.
    ImportRoles {
        /// Workforce config file (resolves permission backend).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Path to the edited role-scope CSV.
        #[arg(long)]
        from_csv: PathBuf,
        /// Actually write rows. Without this flag the command only
        /// prints what would change.
        #[arg(long)]
        apply: bool,
    },
    /// Inspect or recover the admin-editable capability matrix. Break-glass for
    /// a self-inflicted lockout (e.g. `permissions_manage` set to `deny` for
    /// `system_admin`): a running hub's reloader applies the change within ~5s,
    /// no restart needed.
    Matrix {
        #[command(subcommand)]
        command: MatrixCommand,
    },
}

#[derive(Subcommand)]
enum MatrixCommand {
    /// List the cells currently overridden away from their built-in default.
    Show {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Clear every override, reverting all cells to their built-in defaults.
    Reset {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Set one cell. ROLE is `member` or `system_admin`; DECISION is `allow`,
    /// `ask`, or `deny`. Setting a cell to its built-in default clears it.
    Set {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        role: String,
        capability: String,
        decision: String,
    },
}

#[derive(Subcommand)]
enum TeamsCommand {
    /// List all configured teams.
    List,
    /// Show details of a specific team.
    Show {
        /// Team ID.
        id: String,
    },
    /// List available team templates.
    Templates,
    /// Show usage statistics for a team.
    Stats {
        /// Team ID.
        id: String,
        /// Time range: today, week, month.
        #[arg(short, long, default_value = "week")]
        range: String,
    },
    /// Create a new team.
    Create {
        /// Team ID.
        id: String,
        /// Team name.
        #[arg(short, long)]
        name: Option<String>,
        /// Built-in template (product, dev, review).
        #[arg(short, long)]
        template: Option<String>,
        /// Coordination pattern.
        #[arg(long, default_value = "hierarchical")]
        pattern: String,
        /// Lead agent ID.
        #[arg(long)]
        lead: Option<String>,
        /// Member agent IDs (comma-separated).
        #[arg(long, value_delimiter = ',')]
        members: Option<Vec<String>>,
        /// Require review.
        #[arg(long)]
        require_review: bool,
    },
    /// Update an existing team.
    Update {
        /// Team ID.
        id: String,
        /// New name.
        #[arg(short, long)]
        name: Option<String>,
        /// New pattern.
        #[arg(long)]
        pattern: Option<String>,
        /// New lead.
        #[arg(long)]
        lead: Option<String>,
    },
    /// Delete a team.
    Delete {
        /// Team ID.
        id: String,
    },
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Generate leaf node configuration for connecting to the hub.
    Config {
        /// Hub NATS URL.
        #[arg(long, default_value = "nats-leaf://localhost:7422")]
        hub_url: String,
        /// Node ID.
        #[arg(long)]
        node_id: Option<String>,
        /// Output path for the generated config.
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Show node information.
    Info,
    /// Start the local node (connect to hub).
    Start {
        /// Hub URL.
        #[arg(long, default_value = "nats-leaf://localhost:7422")]
        hub_url: String,
        /// Capabilities this node provides.
        #[arg(long, value_delimiter = ',')]
        capabilities: Option<Vec<String>>,
        /// Filesystem paths to expose to agents.
        #[arg(long, value_delimiter = ',')]
        paths: Option<Vec<String>>,
    },
    /// Stop the local node.
    Stop,
    /// Show node status (connection, capabilities, running agents).
    Status,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// List installed skills.
    List,
    /// Search for skills in the registry.
    Search {
        /// Search query.
        query: String,
    },
    /// Show details of a skill.
    Show {
        /// Skill name.
        name: String,
    },
    /// Install a skill.
    Install {
        /// Git URL (with an optional #subpath=<dir>) or local skill directory.
        source: String,
        /// Version to install.
        #[arg(long)]
        version: Option<String>,
    },
    /// Uninstall a skill.
    Uninstall {
        /// Skill name.
        name: String,
    },
    /// Generate a default config.toml for a skill.
    Init {
        /// Skill name (e.g., "slack-integration").
        name: String,
        /// Force overwrite existing config.toml.
        #[arg(long)]
        force: bool,
    },
    /// List skills available to install from the configured catalogs.
    Catalog {
        /// Only show this catalog (by name).
        #[arg(long)]
        source: Option<String>,
    },
    /// Install a skill from a configured catalog, or update an installed one.
    CatalogInstall {
        /// Catalog name (from `[[skills.catalogs]]`).
        catalog: String,
        /// Skill name to install.
        name: String,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// List installed plugins.
    List,
    /// Install a plugin.
    Install {
        /// Plugin source (name, git URL, or local path).
        source: String,
    },
    /// Uninstall a plugin.
    Uninstall {
        /// Plugin name.
        name: String,
    },
    /// Show plugin status.
    Status {
        /// Plugin name.
        name: String,
    },
}

#[derive(Subcommand)]
enum CronCommand {
    /// List all scheduled jobs.
    List,
    /// Add a new scheduled job.
    Add {
        /// Job name.
        #[arg(short, long)]
        name: String,
        /// Message/prompt for the job.
        #[arg(short, long)]
        message: String,
        /// Cron expression (e.g. "0 9 * * *").
        #[arg(long)]
        cron: Option<String>,
        /// Relative time for one-shot (e.g. "2h", "30m").
        #[arg(long, name = "in")]
        in_time: Option<String>,
        /// Recurring interval in seconds.
        #[arg(long)]
        every: Option<u64>,
        /// Timezone for cron (IANA name, e.g. "Europe/London").
        #[arg(long, default_value = None)]
        tz: Option<String>,
        /// Job description.
        #[arg(short, long)]
        description: Option<String>,
        /// Channel ID to deliver output to. With it the job runs an agent
        /// turn, in a fresh session each run; without it the message is
        /// posted as it is.
        #[arg(long)]
        channel: Option<String>,
        /// Model override.
        #[arg(long)]
        model: Option<String>,
    },

    /// Remove a scheduled job.
    Remove {
        /// Job ID.
        job_id: String,
    },
    /// Trigger a job to run immediately.
    Run {
        /// Job ID.
        job_id: String,
    },
    /// Show run history.
    History {
        /// Job ID (optional, show all if omitted).
        job_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum VaultCommand {
    /// Initialize a new vault (creates vault file and key file).
    Init {
        /// Data directory.
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    /// Store a secret in the vault.
    Set {
        /// Dot-notation path (e.g., "slack.bot_token").
        path: String,
        /// Secret value. If omitted, reads from stdin.
        value: Option<String>,
        #[command(flatten)]
        local: LocalVaultArgs,
        /// Gateway URL for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Retrieve a secret from the vault.
    Get {
        /// Dot-notation path (e.g., "slack.bot_token").
        path: String,
        #[command(flatten)]
        local: LocalVaultArgs,
        /// Gateway URL for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// List all secret paths (never shows values).
    List {
        /// Optional prefix filter (e.g., "slack.").
        prefix: Option<String>,
        #[command(flatten)]
        local: LocalVaultArgs,
        /// Gateway URL for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
        /// Show provenance — tag each path with its tier
        /// (`global_user_scoped` / `global_shared_bare`, or the `agent_…`
        /// pair with `--agent`). Local mode only.
        #[arg(long)]
        show_source: bool,
    },
    /// Delete a secret from the vault.
    Delete {
        /// Dot-notation path.
        path: String,
        #[command(flatten)]
        local: LocalVaultArgs,
        /// Gateway URL for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token for remote vault access.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Rotate the vault encryption key. With `--config`, seals every secret
    /// of the Postgres vault under a new key; `--new-key-file` (or
    /// WORKFORCE_VAULT_NEW_KEY) also adds a new passphrase, which
    /// `retire-key` makes the only one once every hub has it.
    RotateKey {
        /// Data directory of a file vault.
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
        /// Workforce config whose Postgres vault to rotate.
        #[arg(long)]
        config: Option<PathBuf>,
        /// File holding the new passphrase.
        #[arg(long, requires = "config")]
        new_key_file: Option<PathBuf>,
    },
    /// Import every vault file into Postgres behind a write freeze. Safe to
    /// re-run.
    Migrate {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Decrypt and count everything; freeze and import nothing.
        #[arg(long)]
        dry_run: bool,
        /// Seconds to wait after freezing for in-flight writes to land.
        #[arg(long, default_value_t = 5)]
        settle_secs: u64,
    },
    /// Write every Postgres vault back to its file and freeze Postgres
    /// (rollback). Then set `[vault] backend = "file"` and redeploy.
    Export {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Seconds to wait after freezing Postgres for in-flight writes.
        #[arg(long, default_value_t = 5)]
        settle_secs: u64,
    },
    /// Compare every vault file with its Postgres copy (paths only, never
    /// values). Exits non-zero when a file changed after it was imported.
    Verify {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Show who changed which vault path and when (never values).
    Audit {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// An agent's own vault instead of the global one.
        #[arg(long)]
        agent: Option<String>,
        /// Only records at or after this RFC 3339 time.
        #[arg(long)]
        since: Option<chrono::DateTime<chrono::Utc>>,
        /// How many of the most recent records to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Delete Postgres key wraps the configured passphrase cannot open. Run
    /// after `rotate-key --new-key-file` once every hub has the new
    /// passphrase.
    RetireKey {
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Copy a per-user credential to other users within an agent's vault.
    /// SystemAdmin only — pass a SystemAdmin login JWT as `--token` (the shared
    /// gateway token is rejected for this action). Never prints the value.
    Copy {
        /// Logical credential path (no `users/` prefix), e.g. "skills/xero/client_secret".
        path: String,
        /// Agent whose vault holds the credential.
        #[arg(long)]
        agent: String,
        /// Source tier.
        #[arg(long, default_value = "agent_per_user")]
        from_tier: String,
        /// Destination tier (must be per-user).
        #[arg(long, default_value = "agent_per_user")]
        to_tier: String,
        /// Source user id (required when the source tier is per-user).
        #[arg(long)]
        from_user: Option<String>,
        /// Destination user ids (comma-separated).
        #[arg(long, value_delimiter = ',', required = true)]
        to_users: Vec<String>,
        /// Overwrite existing destination values.
        #[arg(long)]
        overwrite: bool,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
}

#[derive(Subcommand)]
enum LessonsCommand {
    /// Back-extract lessons from historical session JSONL files. Runs the
    /// same detector + extractor + validator that the live path uses.
    Backfill {
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Restrict to a single agent. Repeatable. Omit to scan every agent
        /// under the installation's `data_dir/agents/` directory.
        #[arg(long)]
        agent: Vec<String>,
        /// Only process sessions whose `updated_at` is at or after this RFC
        /// 3339 timestamp (e.g. `2026-01-01T00:00:00Z`).
        #[arg(long)]
        since: Option<DateTime<Utc>>,
        /// Run the full detection + extraction + validation pipeline but
        /// write into a discarded in-memory store, leaving production
        /// Weaviate untouched. Useful for a first pass against a new
        /// extraction prompt.
        #[arg(long)]
        dry_run: bool,
        /// Override the extraction-model selection. Defaults to each turn's
        /// natural resolution (`lesson_extract_model → passive_model →
        /// compaction model`).
        #[arg(long)]
        model: Option<String>,
        /// Print the full content + tags of every lesson written during
        /// this run, grouped by agent. Pair with `--dry-run` for a
        /// human-readable eyeball before committing to a real write.
        #[arg(long)]
        print_lessons: bool,
    },
    /// Groom the stored lesson corpus in place: prune generic-etiquette
    /// platitudes (re-applying the live "concrete anchor" bar) and merge
    /// genuine near-duplicate re-extractions. Defaults to a non-destructive
    /// dry run; pass `--commit` to actually delete/merge.
    Cleanup {
        /// Config file path.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Restrict to a single agent. Repeatable. Omit to groom every owner
        /// in the lesson corpus.
        #[arg(long)]
        agent: Vec<String>,
        /// Actually mutate the store (forget pruned lessons, merge duplicates).
        /// Without this the run is read-only and only reports what it would do.
        #[arg(long)]
        commit: bool,
        /// Override the prune-classifier model. Defaults to the provider's
        /// configured default model.
        #[arg(long)]
        model: Option<String>,
        /// Recall-certainty bar for treating two lessons as merge-worthy
        /// near-duplicates. Defaults to the live lesson dedup threshold.
        #[arg(long)]
        merge_similarity: Option<f32>,
        /// Print every prune/merge decision for eyeballing before a `--commit`.
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// List policy proposals.
    List {
        /// Filter by status: pending, approved, rejected.
        #[arg(short, long)]
        status: Option<String>,
        /// Maximum proposals to show.
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
    /// Show details of a specific proposal.
    Show {
        /// Proposal ID.
        id: i64,
    },
    /// Approve a policy proposal.
    Approve {
        /// Proposal ID.
        id: i64,
        /// Optional reviewer note.
        #[arg(short, long)]
        note: Option<String>,
    },
    /// Reject a policy proposal.
    Reject {
        /// Proposal ID.
        id: i64,
        /// Optional reviewer note.
        #[arg(short, long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
enum RoleCommand {
    /// List all role templates.
    List,
    /// Show details of a role template.
    Show {
        /// Role ID.
        id: String,
    },
    /// Create a new role template.
    Create {
        /// Role ID (used as filename).
        id: String,
        /// Human-readable name.
        #[arg(short, long)]
        name: String,
        /// System prompt for the role.
        #[arg(short, long)]
        prompt: String,
        /// Capabilities (comma-separated).
        #[arg(long, value_delimiter = ',')]
        capabilities: Option<Vec<String>>,
        /// Expertise areas (comma-separated).
        #[arg(long, value_delimiter = ',')]
        expertise: Option<Vec<String>>,
        /// Communication style.
        #[arg(long)]
        style: Option<String>,
    },
    /// Delete a role template.
    Delete {
        /// Role ID.
        id: String,
    },
    /// Initialise built-in role templates.
    Init,
}

#[derive(Subcommand)]
enum SlackCommand {
    /// Generate a Slack app manifest.
    Scaffold {
        /// App name.
        #[arg(long)]
        name: String,
        /// Bot display name (defaults to app name).
        #[arg(long)]
        bot_name: Option<String>,
    },
    /// Store Slack credentials in the vault, scoped by workspace ID.
    Configure {
        /// Workspace connection ID (e.g. "default", "company").
        #[arg(long)]
        workspace_id: String,
        /// Bot OAuth token (xoxb-...).
        #[arg(long)]
        bot_token: String,
        /// Signing secret.
        #[arg(long)]
        signing_secret: Option<String>,
        /// App-level token for Socket Mode (xapp-...).
        #[arg(long)]
        app_token: Option<String>,
    },
}

#[derive(Subcommand)]
enum GoogleCommand {
    /// Authorize a Google account (opens browser for OAuth consent).
    Login {
        /// Workspace name (default: 'default'). Use different names for
        /// multiple accounts (e.g. 'work', 'personal').
        #[arg(short, long, default_value = "default")]
        workspace: String,
        /// Skip browser launch and use the device flow instead (for headless
        /// environments / containers).
        #[arg(long)]
        no_browser: bool,
        /// Override OAuth client ID (for BYO Google Cloud projects).
        #[arg(long)]
        client_id: Option<String>,
        /// Override OAuth client secret (for BYO Google Cloud projects).
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// Remove stored tokens for a workspace.
    Logout {
        /// Workspace name.
        #[arg(short, long, default_value = "default")]
        workspace: String,
    },
    /// List authorized Google accounts.
    List,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// List configured agents.
    List,
    /// Show details of an agent.
    Show { id: String },
    /// Create a new agent.
    Create {
        /// Agent ID.
        id: String,
        /// Agent name.
        #[arg(short, long)]
        name: Option<String>,
        /// Role template to use.
        #[arg(short, long)]
        role: Option<String>,
        /// LLM model override.
        #[arg(short, long)]
        /// System prompt.
        #[arg(short, long)]
        prompt: Option<String>,
        /// Capabilities (comma-separated).
        #[arg(long, value_delimiter = ',')]
        capabilities: Option<Vec<String>>,
        /// Set as default agent.
        #[arg(long)]
        default: bool,
    },
    /// Update an existing agent.
    Update {
        /// Agent ID.
        id: String,
        /// New name.
        #[arg(short, long)]
        name: Option<String>,
        /// New model.
        #[arg(short, long)]
        /// New system prompt.
        #[arg(short, long)]
        prompt: Option<String>,
        /// Set as default.
        #[arg(long)]
        default: Option<bool>,
    },
    /// Delete an agent.
    Delete {
        /// Agent ID.
        id: String,
        /// Also delete workspace directory.
        #[arg(long)]
        delete_workspace: bool,
    },
}

#[derive(Subcommand)]
enum HeartbeatCommand {
    /// List all heartbeat checks for an agent.
    List {
        /// Agent ID (use "default" for the default agent workspace).
        #[arg(default_value = "default")]
        agent: String,
    },
    /// Show details of a heartbeat check.
    Show {
        /// Agent ID.
        agent: String,
        /// Heartbeat check name (file stem).
        name: String,
    },
    /// Create a new heartbeat check.
    Create {
        /// Agent ID.
        agent: String,
        /// Check name (used as filename).
        name: String,
        /// Human-readable display name.
        #[arg(long)]
        display_name: Option<String>,
        /// Cron schedule expression.
        #[arg(long)]
        schedule: Option<String>,
        /// Priority (lower = higher priority).
        #[arg(long, default_value = "5")]
        priority: u32,
        /// Check body/instructions.
        #[arg(long)]
        body: Option<String>,
    },
    /// Edit a heartbeat check.
    Edit {
        /// Agent ID.
        agent: String,
        /// Check name.
        name: String,
        /// New schedule.
        #[arg(long)]
        schedule: Option<String>,
        /// New priority.
        #[arg(long)]
        priority: Option<u32>,
        /// Enable/disable.
        #[arg(long)]
        enabled: Option<bool>,
        /// New body.
        #[arg(long)]
        body: Option<String>,
    },
    /// Enable a heartbeat check.
    Enable {
        /// Agent ID.
        agent: String,
        /// Check name.
        name: String,
    },
    /// Disable a heartbeat check.
    Disable {
        /// Agent ID.
        agent: String,
        /// Check name.
        name: String,
    },
    /// Delete a heartbeat check.
    Delete {
        /// Agent ID.
        agent: String,
        /// Check name.
        name: String,
    },
    /// Initialize built-in heartbeat check templates.
    Init {
        /// Agent ID.
        #[arg(default_value = "default")]
        agent: String,
    },
}

#[derive(Subcommand)]
enum PairCommand {
    /// List pending pairing requests.
    Pending {
        /// Filter by kind: channel, device, node.
        #[arg(long)]
        kind: Option<String>,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Approve a pairing request by code.
    Approve {
        /// The 8-character pairing code.
        code: String,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Reject a pairing request by code.
    Reject {
        /// The 8-character pairing code.
        code: String,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// List the channel allowlist.
    Allowlist {
        /// Filter by platform (slack, discord, ws, telegram).
        #[arg(long)]
        platform: Option<String>,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Manually add a sender to the channel allowlist.
    Allow {
        /// Platform (slack, discord, ws, telegram).
        platform: String,
        /// Sender ID (platform-specific user ID).
        sender_id: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a sender from the channel allowlist.
    Revoke {
        /// Platform.
        platform: String,
        /// Sender ID.
        sender_id: String,
    },
    /// List paired devices.
    Devices {
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Revoke a paired device.
    RevokeDevice {
        /// Device ID.
        device_id: String,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Gateway auth token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Show pairing configuration summary.
    Config,
    /// Request a device pairing code (for teammates — no admin token needed).
    Request {
        /// Device name for this pairing request.
        #[arg(long)]
        name: Option<String>,
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
    },
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Serve the runner protocol and keep scopes in step with the state file.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Refresh the golden datasets (all, one by name, or those whose watched
    /// files changed) and prune old snapshots; exits non-zero when any fails.
    Refresh {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, conflicts_with = "changed")]
        golden: Option<String>,
        /// Refresh only the goldens whose watched files changed on their
        /// branch since their last refresh, and the databases seeded from them.
        #[arg(long)]
        changed: bool,
    },
    /// Replicate the goldens and caches into the backup pool, and to the
    /// bucket on the weekly day.
    Backup {
        #[arg(long)]
        config: PathBuf,
    },
    /// Report whether this host is fit to serve; exits non-zero on any failure.
    Doctor {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum LocalAgentCommand {
    /// Connect to a gateway and execute commands locally.
    Connect {
        /// Gateway URL (or set WORKFORCE_GATEWAY_URL env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Authentication token (or set WORKFORCE_GATEWAY_TOKEN env var).
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
        /// Unique ID for this local agent: 1-24 lowercase letters, digits and
        /// dashes (defaults to the host name's first label). Its tools reach
        /// the hub as `node.<id>.<tool>`.
        #[arg(long)]
        id: Option<String>,
        /// Human-readable name for this agent.
        #[arg(long)]
        name: Option<String>,
        /// Working directory for command execution.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// A streamable-HTTP MCP endpoint on this machine whose tools to offer
        /// the hub, e.g. a local hub's `http://127.0.0.1:8091/mcp/agents/<id>`.
        /// Its bearer is read from WORKFORCE_NODE_MCP_TOKEN or `--mcp-token-file`.
        #[arg(long, requires = "mcp_tools")]
        mcp: Option<String>,
        /// Which of the MCP server's tools to offer, comma-separated: exact names
        /// or prefixes ending in `*` (e.g. `desk_*,filesystem_read`).
        #[arg(long, value_delimiter = ',', requires = "mcp")]
        mcp_tools: Vec<String>,
        /// A file holding the MCP endpoint's bearer token, when
        /// WORKFORCE_NODE_MCP_TOKEN is not set.
        #[arg(long, requires = "mcp")]
        mcp_token_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum WorkflowCommand {
    /// List available workflow definitions.
    List {
        /// Gateway URL (if set, lists remote workflows).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Validate a workflow TOML file.
    Validate {
        /// Path to the TOML file to validate.
        path: PathBuf,
    },
    /// Show details of a workflow definition.
    Show {
        /// Workflow ID.
        id: String,
        /// Gateway URL (if set, shows remote workflow).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// List workflow instances.
    Instances {
        /// Filter by status: running, paused, completed, failed, exited.
        #[arg(short, long)]
        status: Option<String>,
        /// Filter by workflow ID.
        #[arg(short, long)]
        workflow: Option<String>,
    },
    /// Show details of a workflow instance.
    Status {
        /// Instance ID.
        id: String,
    },
    /// Scaffold a new workflow TOML file locally.
    Create {
        /// Workflow ID (used as filename: {id}.toml).
        id: String,
    },
    /// Download workflow(s) from the server.
    Pull {
        /// Workflow IDs to download. If empty, downloads all.
        ids: Vec<String>,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
    /// Upload workflow(s) to the server.
    Push {
        /// Paths to TOML files to upload. If empty, pushes all in workflows dir.
        paths: Vec<PathBuf>,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
    /// Delete a workflow from the server.
    Delete {
        /// Workflow ID to delete.
        id: String,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
}

#[derive(Subcommand)]
enum PlaybookCommand {
    /// List playbook definitions (local, or remote with --gateway).
    List {
        /// Gateway URL (if set, lists remote playbooks).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
    },
    /// Validate a playbook TOML file.
    Validate {
        /// Path to the TOML file to validate.
        path: PathBuf,
    },
    /// Show a playbook definition's steps (local, or remote with --gateway).
    Show {
        /// Playbook id.
        id: String,
        /// Gateway URL (if set, shows the remote playbook).
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: Option<String>,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: Option<String>,
        /// Owner identity for a member-owned playbook (omit for system/global).
        #[arg(long)]
        owner: Option<String>,
    },
    /// Scaffold a new playbook TOML file locally.
    Create {
        /// Playbook id (used as the filename: {id}.toml).
        id: String,
    },
    /// Download playbook(s) from the server.
    Pull {
        /// Playbook ids to download. If empty, downloads all.
        ids: Vec<String>,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
        /// Owner identity for member-owned playbooks (omit for system/global).
        /// Applies only when downloading specific ids, not the pull-all form.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Upload playbook(s) to the server.
    Push {
        /// Paths to TOML files to upload. If empty, pushes all in the playbooks dir.
        paths: Vec<PathBuf>,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
    /// Delete a playbook from the server.
    Delete {
        /// Playbook id to delete.
        id: String,
        /// Gateway URL.
        #[arg(long, env = "WORKFORCE_GATEWAY_URL")]
        gateway: String,
        /// Auth token.
        #[arg(long, env = "WORKFORCE_GATEWAY_TOKEN")]
        token: String,
    },
}

/// Where the vectors of a seeding run are made.
#[derive(clap::Args, Debug, Clone, PartialEq)]
struct EmbedArgs {
    /// An embedding endpoint that makes the vectors instead of the vellev
    /// node: one serving the node's own model, BAAI/bge-base-en-v1.5, with
    /// no prefix for documents, as text-embeddings-inference does. It is
    /// compared with the node over a sample before anything is written,
    /// and refused if the two do not embed alike. Without it the node
    /// embeds, one run of its model at a time.
    #[arg(long)]
    embed_url: Option<String>,
    /// The endpoint's API: "tei" (`POST /embed`) or "openai"
    /// (`POST /v1/embeddings`).
    #[arg(long, default_value = "tei")]
    embed_api: String,
    /// The model the endpoint is asked for, where its API names one.
    #[arg(long, default_value = "BAAI/bge-base-en-v1.5")]
    embed_model: String,
    /// The environment variable holding the endpoint's key, if it has one.
    #[arg(long)]
    embed_key_env: Option<String>,
    /// Objects per request, to the endpoint and to vellev: no more than
    /// the endpoint takes in one (text-embeddings-inference: 32, unless
    /// run with a larger --max-client-batch-size).
    #[arg(long, default_value_t = 32)]
    embed_batch: usize,
    /// Requests the endpoint has at once, over every page and repo being
    /// written.
    #[arg(long, default_value_t = 8)]
    embed_concurrency: usize,
    /// The most texts the endpoint is sent at once, over all requests:
    /// what it was started to hold (text-embeddings-inference:
    /// --max-concurrent-requests, which counts texts and is 512 unless set
    /// otherwise). --embed-concurrency is lowered to keep within it; an
    /// endpoint sent more answers 429, and is asked again more slowly.
    #[arg(long, default_value_t = 512)]
    embed_inputs_in_flight: usize,
}

impl EmbedArgs {
    /// The outside embedder these name, if any.
    fn seeding(&self) -> Result<Option<workforce_vellev::seeding::Seeding>> {
        let Some(url) = &self.embed_url else {
            return Ok(None);
        };
        let key = match &self.embed_key_env {
            Some(name) => Some(
                std::env::var(name)
                    .map_err(|_| eyre::eyre!("--embed-key-env names {name}, which is not set"))?,
            ),
            None => None,
        };
        let embedder = workforce_hub::seeding::Outside::new(
            url,
            self.embed_api.parse()?,
            &self.embed_model,
            key,
        )?;
        let (batch, concurrency) = self.requests();
        if concurrency < self.embed_concurrency {
            println!(
                "  --embed-concurrency lowered from {} to {concurrency}: {batch} texts a request, and the endpoint is to be sent no more than {} at once (--embed-inputs-in-flight)",
                self.embed_concurrency, self.embed_inputs_in_flight
            );
        }
        Ok(Some(workforce_vellev::seeding::Seeding::new(
            std::sync::Arc::new(embedder),
            batch,
            concurrency,
        )))
    }

    /// Texts to a request, and requests at once: as asked, the requests
    /// fewer if together they would hold more texts than the endpoint is
    /// to be sent at once.
    fn requests(&self) -> (usize, usize) {
        let batch = self.embed_batch.max(1);
        let fit = (self.embed_inputs_in_flight / batch).max(1);
        (batch, self.embed_concurrency.clamp(1, fit))
    }
}

/// Say what comparing an outside embedder with the node came to.
fn print_equivalence(measured: &workforce_vellev::seeding::Equivalence) {
    println!(
        "  the embedding endpoint embeds as the node does: over {} texts the least cosine is {:.5}, the mean {:.5}",
        measured.compared, measured.lowest, measured.mean
    );
}

#[derive(Subcommand)]
enum ArtifactsCommand {
    /// Bring the configured vector index of artifacts (vellev's or
    /// Weaviate's) in line with Postgres: index what it lacks or holds
    /// another text of, remove what is gone, then mark the index complete
    /// so that artifact search relies on it. Safe to run while the hub
    /// serves, and again after a run that stopped.
    Reindex {
        /// Workforce config file (reads `[postgres]`, `[artifacts]`,
        /// `[vellev]` and `[weaviate]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// Fill the vellev mirror kept beside a shadowed artifact index
        /// (`[artifacts] index_backend = "shadow"`) rather than the index
        /// the hub serves. Run it before the shadow is turned on: like any
        /// seeding it is refused while a hub keeps that index.
        #[arg(long)]
        mirror: bool,
        #[command(flatten)]
        embed: EmbedArgs,
    },
}

#[derive(Subcommand)]
enum CodeCommand {
    /// Fill vellev's code vectors from the code graph, with no crawl:
    /// every entity and summary of every indexed repo, a page at a time.
    /// It keeps its place, so a run that is stopped goes on when run
    /// again; nothing is deleted first; and it may run before a cutover
    /// (set WORKFORCE_KNOWLEDGE_VECTOR_BACKEND=vellev for the run) or
    /// alongside the hub. It ends by counting what vellev holds against
    /// the graph, and fails if any repo is incomplete.
    ReindexVectors {
        /// Workforce config file (reads `[knowledge]`, `[codegraph]`,
        /// `[postgres]` and `[vellev]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
        /// One repo, by its id in the code graph; every indexed repo
        /// otherwise.
        #[arg(long)]
        repo: Option<String>,
        /// The directory the code graph is kept under, instead of
        /// `[knowledge] data_dir`. A vellev tenant holds one code graph's
        /// vectors: where agents keep graphs of their own, in data
        /// directories of their own, only one of them can keep its vectors
        /// in the tenant.
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// The most entities and summaries stored per second; 0 for no
        /// limit. By default `[knowledge] vector_backfill_per_sec` when
        /// the node embeds, so that questions are not kept waiting, and no
        /// limit with --embed-url.
        #[arg(long)]
        rate: Option<u32>,
        /// Fill every repo (or --repo) again from the code graph, whatever
        /// is recorded of its vectors: every vector is written over and
        /// what the graph has not is removed at the end. Its vectors go on
        /// being searched meanwhile if they were complete. For when the
        /// vectors are known or feared to be wrong, as after vellev was
        /// restored from a backup; without it, repos recorded complete are
        /// compared with the graph and only what differs is put right.
        #[arg(long)]
        force: bool,
        /// Take the tenant's code vectors over for this hub's code graph,
        /// and fill them again (as --force): for when the code graph they
        /// were kept for is no more, its database having been made again.
        #[arg(long)]
        take_over: bool,
        /// Repos filled at once. By default one when the node embeds, and
        /// four with --embed-url.
        #[arg(long)]
        repos_at_once: Option<usize>,
        #[command(flatten)]
        embed: EmbedArgs,
    },
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// Validate the WorkforceMemory Weaviate class shape against the
    /// expected property set. Exits non-zero on mismatch. Read-only.
    Doctor {
        /// Workforce config file (reads `[weaviate]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Rebuild the WorkforceMemory class end-to-end: export all objects,
    /// delete the class, ensure_collection, re-ingest. Destructive.
    RebuildSchema {
        /// Confirm the destructive operation. Without `--yes` this is a
        /// dry-run that prints the plan and the live schema shape.
        #[arg(long)]
        yes: bool,
        /// Path to write the pre-delete backup JSON. Defaults to a
        /// timestamped file under the workdir.
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Workforce config file (reads `[weaviate]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Audit the WorkforceMemory class for known pollution patterns and
    /// optionally tag each flagged memory with a `kind:` classification so
    /// future recalls can exclude them. Default = dry-run.
    ///
    /// Detects:
    ///   * `kind:recap_artifact` — clusters of memories from one recap turn
    ///   * `kind:consolidation_meta` — agent self-reports of consolidation
    ///
    /// Without `--apply` the command writes a JSON manifest you can review.
    /// With `--apply` it PATCHes each flagged object's `tags` in place.
    Clean {
        /// Apply the tag changes. Default is dry-run.
        #[arg(long)]
        apply: bool,
        /// Where to write the manifest JSON. Defaults to a timestamped file
        /// in the current directory.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Workforce config file (reads `[weaviate]`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Pre-seed the per-agent consolidation cursor so the next scheduled
    /// scan reprocesses every session updated on or after `--since`.
    ///
    /// Use this to recover memory after a known window of dropped
    /// extractions (incidents, regression bugs, manual data ops). Idempotent
    /// — re-running with the same `--since` produces no additional work.
    ///
    /// This only nudges the cursor; the work happens on the next scheduled
    /// consolidation tick (bounded by its session-budget). For an immediate,
    /// unbounded re-extraction of every turn in the window, see
    /// `memory passive-extract` instead.
    Backfill {
        /// ISO-8601 timestamp (e.g. `2026-05-01T00:00:00Z`). The next scan
        /// will pick up every session whose `updated_at >= since`.
        #[arg(long)]
        since: String,
        /// Agent ID whose cursor should be reset (e.g. `allan`).
        #[arg(long)]
        agent: String,
        /// Workforce config file (reads `[hub] data_dir`).
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Directly re-extract passive memories from every session turn since
    /// `--since`, bypassing the consolidation cursor and budget. Heavier
    /// than `backfill` (one LLM call per turn) but recovers memories
    /// regardless of session-count budget.
    ///
    /// Dedup notes:
    /// - The passive extractor's near-duplicate check uses cosine similarity
    ///   ≥ 0.92 on content. The fact-grade prompt upgrade (see passive
    ///   prompt) appends a `Facts:` block to extracted memories — for
    ///   turns that already produced facts-less memories the similarity
    ///   may fall below 0.92 and yield a second copy. To avoid this, point
    ///   `--since` BEFORE the regression window so the run extracts on a
    ///   clean baseline rather than alongside the existing memories.
    /// - `--dry-run` exercises the gate + extractor without writing.
    ///
    /// Use when consolidation alone won't catch up — typically after a
    /// silent-failure window (e.g. the live-extraction regression fixed in
    /// commit 8872565). For a cheap cursor-only reset, see `memory backfill`.
    PassiveExtract {
        /// ISO-8601 timestamp. Sessions older than this are skipped.
        #[arg(long)]
        since: String,
        /// Agent IDs to scan. Repeat the flag for multiple agents. Empty =
        /// every agent under `[hub] data_dir/agents/`.
        #[arg(long)]
        agent: Vec<String>,
        /// Don't write to the store — exercise extraction only.
        #[arg(long)]
        dry_run: bool,
        /// Optional extractor model override (workforce tier spec, e.g.
        /// `cheap` or `openrouter:deepseek:cheap`).
        #[arg(long)]
        model: Option<String>,
        /// Workforce config file.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Move the customer-facing edge's memories out of the hub's stores
    /// into the edge's own vellev tenant (DW-676 (b)), in two steps run in
    /// this order: `copy`, with the edge stopped, then `purge`, once the
    /// edge runs on its tenant. Each is a dry run without `--apply`, and
    /// safe to run again. Needs `[memory] backend = "shadow"` and
    /// `[embeddings] keep_previous = false` on every hub. The edge's
    /// tenant key is read from EDGE_VELLEV_API_KEY, never the command line.
    MoveEdge {
        /// `copy` or `purge`.
        #[arg(value_enum)]
        step: MoveEdgeStep,
        /// Write; without it nothing is changed and counts are printed.
        #[arg(long)]
        apply: bool,
        /// The edge's agent ids, as its `[[agents]] id`; repeat for each.
        #[arg(long = "agent", required = true)]
        agents: Vec<String>,
        /// The edge's `[vellev] url`.
        #[arg(long)]
        edge_url: String,
        /// The edge's `[vellev] tenant`.
        #[arg(long)]
        edge_tenant: String,
        /// The edge's `[vellev] model`.
        #[arg(long)]
        edge_model: String,
        /// The edge's `[vellev] dimensions`.
        #[arg(long)]
        edge_dimensions: u32,
        /// The edge's `[vellev] query_prefix`, if it sets one.
        #[arg(long)]
        edge_query_prefix: Option<String>,
        /// For `copy`: a file of every customer the edge still knows, one
        /// owner key a line, from the edge's database
        /// (`SELECT owner FROM edge_analytics.chat_identities`). A customer's
        /// memory of anyone else is taken as erased: purged, never copied.
        #[arg(long)]
        known_customers: Option<PathBuf>,
        /// Workforce config file.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Move the memories a hub kept in its SQLite file into vellev, the
    /// store `[memory] backend = "vellev"` serves them from, keeping their
    /// ids; then read every one back. Run it with the hub stopped and its
    /// config already set to vellev. A dry run without `--apply`, and safe
    /// to run again. Exits non-zero unless every memory with something in
    /// it was moved and read back as it was. The SQLite file is only read.
    MoveSqlite {
        /// The SQLite memory file (`[memory] db_path` before the move).
        #[arg(long)]
        from: PathBuf,
        /// Write; without it nothing is changed and counts are printed.
        #[arg(long)]
        apply: bool,
        /// Print the counts as one line of JSON.
        #[arg(long)]
        json: bool,
        /// Workforce config file.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
    /// Code-only relationship scan over historical DM transcripts.
    ///
    /// Scores any human DM turns past each session's cursor and records them as
    /// relationship events (no LLM call). Idempotent: the per-session cursor
    /// means a re-run only picks up new turns. Live capture covers turns going
    /// forward; this catches up history once relationship tracking is enabled.
    RelationshipScan {
        /// ISO-8601 timestamp. Sessions older than this are skipped.
        #[arg(long)]
        since: String,
        /// Agent IDs to scan. Repeat the flag for multiple agents. Empty =
        /// every agent under `[hub] data_dir/agents/`.
        #[arg(long)]
        agent: Vec<String>,
        /// Score against a throwaway store instead of the real telemetry store.
        #[arg(long)]
        dry_run: bool,
        /// Workforce config file.
        #[arg(short, long, default_value = "workforce.toml")]
        config: PathBuf,
    },
}

/// A step of `memory move-edge`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum MoveEdgeStep {
    Copy,
    Purge,
}

/// Write identity template files into a workspace directory.
///
/// Creates the workspace and `memory/` subdirectory, then writes template
/// identity files (SOUL.md, AGENTS.md, etc.) — skipping any that already
/// exist. Per-user context lives in `users/{identity_uuid}.md`, auto-created
/// by [`workforce_hub::user_discovery`] on first interaction, so no global
/// USER.md is seeded.
fn write_identity_files(workspace: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(workspace)?;
    std::fs::create_dir_all(workspace.join("memory"))?;

    let identity_files: &[(&str, &str)] = &[
        (
            "SOUL.md",
            "# SOUL.md — Who You Are\n\n\
             *Define your agent's personality, values, and communication style here.*\n\n\
             ## Core Traits\n\n\
             - Be genuinely helpful, not performatively helpful\n\
             - Be concise when needed, thorough when it matters\n\
             - Have opinions — an assistant with no personality is just a search engine\n\n\
             ## Vibe\n\n\
             Be the assistant you'd actually want to talk to.\n",
        ),
        (
            "TOOLS.md",
            "# TOOLS.md — Tool Notes\n\n\
             *Local notes about tools, services, SSH hosts, API endpoints, etc.*\n",
        ),
        (
            "AGENTS.md",
            "# AGENTS.md — Session Instructions\n\n\
             *How your agent should behave at the start of each session.*\n\n\
             ## Every Session\n\n\
             1. Read `SOUL.md` — this is who you are\n\
             2. Use `messaging.whoami` and the prompt's \"Talking To\" block to know who you're helping\n\
             3. Read `memory/` daily notes for recent context\n\
             4. Check `memory/active-task.md` — resume if mid-task\n\
             5. Use `memory(action=\"recall\")` for long-term context\n\n\
             ## Memory\n\n\
             You wake up fresh each session. These are your continuity:\n\
             - **Daily notes:** `memory/YYYY-MM-DD.md` — raw logs of what happened\n\
             - **Long-term:** the `memory` and `notes` tools, dispatched by `action` (`memory(action=\"recall\")` / `notes(action=\"append_memory\")`)\n\
             - **Active task:** `memory/active-task.md` — what you were working on\n\n\
             Capture what matters. Decisions, context, things to remember.\n",
        ),
        (
            "IDENTITY.md",
            "# IDENTITY.md — Who Am I?\n\n\
             - **Name:** \n\
             - **Creature:** \n\
             - **Vibe:** \n\
             - **Emoji:** \n\n\
             *Give your agent a name and personality.*\n",
        ),
        (
            "HEARTBEAT.md",
            "# HEARTBEAT.md\n\n\
             # Keep this file empty (or with only comments) to skip heartbeat work.\n\
             # Add tasks below when you want the agent to check something periodically.\n",
        ),
        (
            "WELCOME.md",
            "Read your IDENTITY.md and SOUL.md to know who you are.\n\
             If the memory/ directory has any files, briefly mention what's in progress.\n\
             If memory/ is empty, just introduce yourself based on your identity.\n\
             Keep it to 2-3 sentences. Don't mention or apologise about empty files.\n",
        ),
    ];

    let mut created = Vec::new();
    for (filename, content) in identity_files {
        let path = workspace.join(filename);
        if !path.exists() {
            std::fs::write(&path, content)?;
            created.push(filename.to_string());
        } else {
            eprintln!(
                "  {} {} already exists, skipping",
                style("↩").dim(),
                path.display()
            );
        }
    }
    // Always list the memory/ directory
    created.push("memory/".to_string());
    Ok(created)
}

/// Resolve the provider kind and API key from CLI args and env vars.
fn resolve_provider_and_key(
    provider_str: &str,
    explicit_key: Option<String>,
) -> Result<(ProviderKind, String)> {
    let kind = ProviderKind::parse(provider_str).map_err(|e| match e {
        removed @ ProviderParseError::Removed { .. } => eyre::eyre!(removed),
        ProviderParseError::Unknown(_) => eyre::eyre!(
            "Unknown provider '{}'. Use 'anthropic', 'openrouter', 'grok', 'gemini', or 'ollama'.",
            provider_str
        ),
    })?;

    let api_key = if let Some(key) = explicit_key {
        key
    } else {
        match kind {
            // BedrockClient and VertexAnthropicClient resolve credentials
            // via their own chains (AWS env vars / Google ADC); the api_key
            // slot here is just a passthrough.
            ProviderKind::Bedrock => String::new(),
            ProviderKind::VertexAnthropic => {
                std::env::var("GOOGLE_APPLICATION_CREDENTIALS").unwrap_or_default()
            }
            _ => provider_env_key(kind).ok_or_else(|| {
                let hint = match kind {
                    ProviderKind::Gemini => {
                        "Set GEMINI_API_KEY (or GOOGLE_API_KEY) env var or use --api-key"
                            .to_string()
                    }
                    _ => format!(
                        "Set {} env var or use --api-key",
                        provider_env_var_hint(kind)
                    ),
                };
                eyre::eyre!(hint)
            })?,
        }
    };

    Ok((kind, api_key))
}

/// Wrap an LLM provider with failover if the config has failover/aliases.
fn wrap_with_failover(
    llm: Box<dyn LlmProvider>,
    config: Option<&workforce_common::settings::WorkforceConfig>,
) -> Box<dyn LlmProvider> {
    // The CLI has no `PooledProvider`, so the liveness watchdog the hub
    // gets from its pool must be applied here — to the bare client, under
    // any failover wrap, so every chain attempt is enforced.
    let timeouts = config
        .and_then(|c| c.llm.retry.as_ref())
        .map(workforce_agent::llm::failover::LlmTimeouts::from)
        .unwrap_or_default();
    let llm: Box<dyn LlmProvider> = Box::new(
        workforce_agent::llm::provider::WatchdogProvider::new(llm, timeouts),
    );
    if let Some(cfg) = config {
        if cfg.llm.has_failover_config() {
            let failover_config = FailoverConfig {
                failover: cfg.llm.failover.clone(),
                aliases: cfg.llm.aliases.clone(),
                retry: cfg
                    .llm
                    .retry
                    .as_ref()
                    .map(|r| workforce_agent::llm::failover::RetryConfig {
                        max_retries: r.max_retries,
                        initial_backoff_ms: r.initial_backoff_ms,
                        backoff_multiplier: r.backoff_multiplier,
                        max_backoff_ms: r.max_backoff_ms,
                        retry_overloaded: r.retry_overloaded,
                        retry_rate_limited: r.retry_rate_limited,
                        max_rate_limit_wait_secs: r.max_rate_limit_wait_secs,
                        timeouts: r.into(),
                    })
                    .unwrap_or_default(),
                health: cfg
                    .llm
                    .health
                    .as_ref()
                    .map(|h| workforce_agent::llm::failover::HealthConfig {
                        failure_threshold: h.failure_threshold,
                        window_secs: h.window_secs,
                        recovery_secs: h.recovery_secs,
                    })
                    .unwrap_or_default(),
            };
            eprintln!(
                "  {} Model failover enabled (chain: {:?})",
                style("🔄").bold(),
                failover_config.failover
            );
            return Box::new(
                FailoverProvider::new(Arc::from(llm), failover_config)
                    .with_hub_default_model(cfg.llm.default_model.clone()),
            );
        }
    }
    llm
}

/// Resolve the effective model spec for a backfill/extract command.
///
/// Falls back to the configured `llm.default_model` when `--model` is omitted,
/// then routes the spec through [`TierRegistry`] so tier shorthand like
/// `standard` or `openrouter:deepseek:powerful` becomes a concrete model id
/// the provider can dispatch on. Without this fallback a missing `--model`
/// reaches the provider as `None`, which OpenRouter resolves to whichever
/// free-tier compute slot happens to be active and 400s on the first call.
///
/// Returns the resolved id wrapped in `Some` so callers can hand it straight
/// to `extract_and_store(model: Option<String>, …)`. Emits a one-line
/// `eprintln!` whenever the input spec differs from the resolved id so
/// operators can see what the dispatch will actually target.
fn resolve_extract_model(
    explicit: Option<&str>,
    default: &str,
    default_tier_key: &str,
) -> Option<String> {
    let trimmed_explicit = explicit.map(str::trim).filter(|s| !s.is_empty());
    let spec = trimmed_explicit.unwrap_or(default);
    let tiers = workforce_agent::llm::tiers::TierRegistry::new(None);
    let (_routing, resolved) = tiers.resolve_routed_model(spec, default_tier_key);
    if resolved != spec {
        let source = if trimmed_explicit.is_some() {
            "--model"
        } else {
            "config"
        };
        eprintln!(
            "{} Resolved {} `{}` → `{}`",
            style("ℹ").dim(),
            source,
            spec,
            style(&resolved).cyan()
        );
    }
    Some(resolved)
}

/// Where a local `workforce vault` command reads and writes.
#[derive(clap::Args)]
struct LocalVaultArgs {
    /// Data directory holding `vault.enc` (without `--config`).
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// Workforce config: use its vault backend (files or Postgres) instead
    /// of `--data-dir`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// An agent's own vault instead of the global one (needs `--config`).
    #[arg(long, requires = "config")]
    agent: Option<String>,
}

/// The vault a local `workforce vault` command acts on: with `--config`, the
/// configured backend's global vault (or `--agent`'s); otherwise the file in
/// `--data-dir`. Writing a vault file beside a running hub is refused unless
/// allowed; Postgres writes are safe while hubs run.
async fn local_vault(
    args: &LocalVaultArgs,
    mutation: Option<&str>,
) -> Result<Arc<workforce_hub::vault::Vault>> {
    use workforce_hub::vault::{open_configured_agent, open_global, uses_postgres, Vault};
    let Some(config_path) = &args.config else {
        if let Some(op) = mutation {
            guard_local_vault_mutation(op);
        }
        let master_key = Vault::resolve_master_key(None, &args.data_dir)?;
        return Ok(Arc::new(Vault::open(
            &args.data_dir.join("vault.enc"),
            &master_key,
        )?));
    };
    let config = workforce_common::settings::WorkforceConfig::load(config_path)?;
    if !uses_postgres(&config)? {
        if let Some(op) = mutation {
            guard_local_vault_mutation(op);
        }
    }
    let vault = match &args.agent {
        Some(agent) => open_configured_agent(&config, agent).await?,
        None => open_global(&config).await?,
    };
    vault.ok_or_else(|| eyre::eyre!("the vault is disabled or could not be opened"))
}

/// The passphrase `vault rotate-key` moves to: `--new-key-file`, else
/// `WORKFORCE_VAULT_NEW_KEY`; `None` keeps the current passphrase.
fn new_vault_passphrase(file: Option<&std::path::Path>) -> Result<Option<String>> {
    match file {
        Some(file) => {
            let passphrase = std::fs::read_to_string(file)
                .map_err(|e| eyre::eyre!("read {}: {e}", file.display()))?
                .trim()
                .to_string();
            if passphrase.is_empty() {
                eyre::bail!("{} holds no passphrase", file.display());
            }
            Ok(Some(passphrase))
        }
        None => Ok(std::env::var("WORKFORCE_VAULT_NEW_KEY")
            .ok()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())),
    }
}

fn print_vault_reports(reports: &[workforce_hub::vault::migrate::ScopeReport]) {
    use workforce_hub::vault::migrate::Outcome;
    reports.iter().for_each(|report| {
        let outcome = match &report.outcome {
            Outcome::Imported { entries, audit } => {
                format!("imported {entries} secret(s), {audit} audit record(s)")
            }
            Outcome::WouldImport { entries, audit } => {
                format!("would import {entries} secret(s), {audit} audit record(s)")
            }
            Outcome::AlreadyOnPostgres => "already on Postgres".to_string(),
            Outcome::NoFile => "no file; starts empty".to_string(),
            Outcome::Diverged { entries } => format!(
                "NOT imported: Postgres already holds {entries} secret(s) a hub wrote after \
                 creating this vault empty"
            ),
            Outcome::Exported { entries, audit } => {
                format!("exported {entries} secret(s), appended {audit} audit record(s)")
            }
        };
        println!(
            "  {:<24} {outcome}  {}",
            style(&report.scope).cyan(),
            style(report.file.display()).dim()
        );
    });
}

/// Run an interactive chat session through the gateway (WebSocket).
/// Kept as fallback — the TUI version (`tui::run`) is used by default.
#[allow(dead_code)]
/// Guard the local (on-disk) vault **mutation** path. A serving hub loads
/// `vault.enc` once at boot into an in-memory cache and has no fs-watcher on it,
/// so a direct `--data-dir` write persists to disk and prints success while the
/// authoritative (API-served, in-memory) vault keeps serving the old value until
/// the hub next writes to that file and reloads it. That is the silent "All
/// gone!" footgun from the 2026-06-14 incident: a CLI delete reported success
/// while the credential stayed live. Refuse local-mode vault mutation unless the
/// operator explicitly opts in for offline/bootstrap use — the agent's
/// non-interactive `exec.run` shell won't have the override set, so it can no
/// longer fool itself this way.
fn guard_local_vault_mutation(op: &str) {
    let allowed = std::env::var("WORKFORCE_ALLOW_LOCAL_VAULT_WRITE")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if !allowed {
        eprintln!(
            "{} Refusing to {op} the on-disk vault directly.\n  \
             A running hub serves the vault from memory and keeps serving the \
             old value until it next writes to it\n  (a prior `--data-dir` delete \
             silently no-op'd exactly this way, reporting false success).\n  \
             Use `--gateway <url> --token <token>` to change the authoritative \
             vault.\n  For offline/bootstrap use only, set \
             WORKFORCE_ALLOW_LOCAL_VAULT_WRITE=1.",
            style("❌").bold(),
        );
        std::process::exit(1);
    }
}

/// Refuse to mutate the capability matrix from the CLI unless explicitly
/// allowed. The override store IS live authorization state — a running hub
/// applies a direct write within ~5s, bypassing the gateway's
/// `permissions_manage` gate, so an admin's agent shell must not reach it.
/// Override only for offline break-glass on the host.
fn guard_local_matrix_mutation(op: &str) {
    let allowed = std::env::var("WORKFORCE_ALLOW_LOCAL_MATRIX_WRITE")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if !allowed {
        eprintln!(
            "{} Refusing to {op} the capability matrix from the CLI.\n  \
             This is live authorization state the gateway gates behind the \
             SystemAdmin `permissions_manage` capability; a running hub applies \
             a direct write within ~5s, so the CLI would bypass that gate\n  \
             (e.g. from an agent's shell). Use the admin UI / \
             `PUT /api/capabilities/matrix` instead.\n  For offline break-glass \
             on the host only, set WORKFORCE_ALLOW_LOCAL_MATRIX_WRITE=1.",
            style("❌").bold(),
        );
        std::process::exit(1);
    }
}

// Superseded by the rich TUI client (`tui::run`, called from Commands::Chat).
// Kept for reference; allow dead_code so `clippy -D warnings` stays green.
// TODO: remove this old gateway-chat REPL once the TUI path has settled.
#[allow(dead_code)]
async fn run_gateway_chat(gateway_url: &str, token: &str) -> Result<()> {
    use std::io::Write;
    use workforce_gateway_client::{
        GatewayClient, GatewayClientConfig, GatewayEvent, ServerMessageKind,
    };

    eprintln!(
        "{} Chat via gateway at {} (type /quit to exit, /mode for info)",
        style("💬").bold(),
        style(gateway_url).cyan(),
    );
    eprintln!("{}", style("─".repeat(60)).dim());

    let config = GatewayClientConfig {
        url: gateway_url.to_string(),
        token: token.to_string(),
        ..Default::default()
    };

    let mut client = GatewayClient::new(config);
    let mut events = client
        .take_events()
        .ok_or_else(|| eyre::eyre!("Failed to initialise event receiver"))?;
    client.connect().await?;

    // Wait for the connected event
    match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv()).await {
        Ok(Some(GatewayEvent::Connected)) => {
            eprintln!("  {} Connected to gateway", style("✓").green().bold(),);
        }
        Ok(Some(GatewayEvent::Message(msg))) if msg.is_connected() => {
            eprintln!("  {} Connected to gateway", style("✓").green().bold(),);
        }
        _ => {
            eprintln!(
                "{} Gateway did not send connection acknowledgement",
                style("⚠").yellow(),
            );
        }
    }

    eprintln!("{}", style("─".repeat(60)).dim());

    // Spawn async stdin reader so we can select! between user input and gateway events
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<String>(16);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            eprint!("{} ", console::style("you>").green().bold());
            let _ = std::io::Write::flush(&mut std::io::stderr());
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => break, // EOF or error
                Ok(_) => {
                    if input_tx.blocking_send(line.trim().to_string()).is_err() {
                        break; // Receiver dropped
                    }
                }
            }
        }
    });

    let mut waiting_for_response = false;

    loop {
        tokio::select! {
            // User input
            input = input_rx.recv() => {
                let Some(trimmed) = input else { break };
                if trimmed.is_empty() {
                    continue;
                }

                match trimmed.as_str() {
                    "/quit" | "/exit" | "/q" => break,
                    "/mode" => {
                        let session_id = client.session_id().await;
                        eprintln!(
                            "{} Mode: gateway | URL: {} | Session: {}",
                            style("📡").bold(),
                            style(gateway_url).cyan(),
                            style(session_id.as_deref().unwrap_or("(none)")).dim(),
                        );
                    }
                    "/sessions" => {
                        let rest = workforce_gateway_client::GatewayRestClient::new(gateway_url, token);
                        match rest.sessions().await {
                            Ok(resp) => {
                                let current = client.session_id().await;
                                if resp.sessions.is_empty() {
                                    eprintln!("{} No active sessions", style("📋").bold());
                                } else {
                                    eprintln!("{} Active sessions:", style("📋").bold());
                                    for s in &resp.sessions {
                                        let marker = if current.as_deref() == Some(s.id.as_str()) {
                                            " ← current"
                                        } else {
                                            ""
                                        };
                                        let time_info = s.updated_at.as_deref()
                                            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                                            .map(|dt| {
                                                let ago = chrono::Utc::now().signed_duration_since(dt);
                                                if ago.num_minutes() < 1 { "just now".to_string() }
                                                else if ago.num_hours() < 1 { format!("{}m ago", ago.num_minutes()) }
                                                else if ago.num_days() < 1 { format!("{}h ago", ago.num_hours()) }
                                                else { format!("{}d ago", ago.num_days()) }
                                            })
                                            .unwrap_or_default();
                                        eprintln!(
                                            "  {} — {} msgs, {} tokens ({}) {}{}",
                                            style(&s.id[..8]).cyan(),
                                            s.messages,
                                            s.total_tokens,
                                            s.channel.as_deref().unwrap_or("?"),
                                            style(&time_info).dim(),
                                            style(marker).green(),
                                        );
                                    }
                                }
                            }
                            Err(e) => eprintln!("{} {}", style("Error:").red().bold(), e),
                        }
                    }
                    "/new" => {
                        client.clear_session_id().await;
                        eprintln!(
                            "{} Starting new session (previous session preserved on server)",
                            style("🆕").bold(),
                        );
                    }
                    "/help" => {
                        eprintln!("{} Commands:", style("ℹ").bold());
                        eprintln!("  /quit, /q     — Exit chat");
                        eprintln!("  /mode         — Show connection mode and session info");
                        eprintln!("  /sessions     — List active sessions");
                        eprintln!("  /new          — Start a fresh session");
                        eprintln!("  /switch <id>  — Switch to a different session");
                        eprintln!("  /help         — Show this help");
                    }
                    cmd if cmd.starts_with("/switch ") => {
                        let target = cmd.strip_prefix("/switch ").unwrap().trim();
                        if target.is_empty() {
                            eprintln!("Usage: /switch <session_id>");
                        } else {
                            client.set_session_id(target).await;
                            eprintln!(
                                "{} Switched to session {}",
                                style("🔄").bold(),
                                style(target).cyan(),
                            );
                        }
                    }
                    _ => {
                        // Send message to gateway
                        if let Err(e) = client.send_chat(&trimmed).await {
                            eprintln!("{} {}", style("Error:").red().bold(), e);
                        } else {
                            waiting_for_response = true;
                        }
                    }
                }
            }

            // Gateway events (always listening, even while waiting for input)
            event = events.recv() => {
                let Some(event) = event else { break };
                match event {
                    GatewayEvent::Message(msg) => match msg.kind() {
                        ServerMessageKind::Message => {
                            println!();
                            if let Some(text) = msg.text() {
                                println!("{}", text);
                            }
                            println!();
                            waiting_for_response = false;
                        }
                        ServerMessageKind::Image => {
                            // Terminal can't render images; note it and show any caption.
                            println!();
                            if let Some(text) = msg.text() {
                                println!("{}", text);
                            }
                            println!("[image generated — view in the app]");
                            println!();
                            waiting_for_response = false;
                        }
                        ServerMessageKind::StreamStart => {
                            println!();
                            std::io::stdout().flush().ok();
                        }
                        ServerMessageKind::StreamDelta => {
                            if let Some(text) = msg.text() {
                                print!("{}", text);
                                std::io::stdout().flush().ok();
                            }
                        }
                        ServerMessageKind::StreamEnd => {
                            println!("\n");
                            waiting_for_response = false;
                        }
                        ServerMessageKind::StreamDiscard => {
                            println!();
                            eprintln!("  {}", style("↻ reply restarted").dim());
                        }
                        ServerMessageKind::Typing => {}
                        ServerMessageKind::ToolStart => {
                            if let Some(text) = msg.text() {
                                eprintln!("  {} {}", style("⚙").dim(), style(text).dim());
                            }
                        }
                        ServerMessageKind::ToolEnd => {}
                        ServerMessageKind::Error => {
                            let err = msg.error.as_deref().or(msg.text()).unwrap_or("unknown error");
                            eprintln!("{} {}", style("Error:").red().bold(), err);
                            waiting_for_response = false;
                        }
                        ServerMessageKind::Reminder => {
                            if let Some(text) = msg.text() {
                                eprintln!("\n{}", style(text).bold());
                                // Re-show prompt if we're not waiting for a response
                                if !waiting_for_response {
                                    eprint!("{} ", style("you>").green().bold());
                                    std::io::stderr().flush().ok();
                                }
                            }
                        }
                        ServerMessageKind::PermissionPrompt => {
                            if let (Some(rid), Some(tn)) = (msg.request_id.as_deref(), msg.tool_name.as_deref()) {
                                let reason = msg.text().unwrap_or("");
                                eprintln!(
                                    "\n{} {} needs approval ({}) — {}. Reply with {} or {}.",
                                    style("🔐").bold(),
                                    style(tn).bold(),
                                    style(rid).dim(),
                                    reason,
                                    style(format!("approve {rid}")).green(),
                                    style(format!("reject {rid}")).red(),
                                );
                                if !waiting_for_response {
                                    eprint!("{} ", style("you>").green().bold());
                                    std::io::stderr().flush().ok();
                                }
                            }
                        }
                        ServerMessageKind::ContextUpdate
                        | ServerMessageKind::Reaction
                        | ServerMessageKind::Pong
                        | ServerMessageKind::Connected
                        | ServerMessageKind::CompactStart
                        | ServerMessageKind::CompactEnd
                        | ServerMessageKind::NodeInvoke
                        | ServerMessageKind::TeamUpdate
                        | ServerMessageKind::TurnIndicator
                        | ServerMessageKind::LocalExec
                        | ServerMessageKind::LocalExecResult
                        | ServerMessageKind::ConnectPrompt
                        | ServerMessageKind::Route
                        | ServerMessageKind::Unknown => {}
                    },
                    GatewayEvent::Disconnected { reason } => {
                        eprintln!("\n{} Disconnected: {}", style("⚠").yellow(), reason);
                    }
                    GatewayEvent::Reconnecting { attempt } => {
                        eprintln!("{} Reconnecting (attempt {})...", style("🔄").bold(), attempt);
                    }
                    GatewayEvent::ReconnectFailed => {
                        eprintln!("{} Failed to reconnect to gateway", style("✗").red().bold());
                        break;
                    }
                    GatewayEvent::Connected => {
                        eprintln!("{} Reconnected", style("✓").green().bold());
                    }
                }
            }
        }
    }

    client.disconnect().await;
    eprintln!("\n{} Session ended.", style("👋").bold());
    Ok(())
}

/// Build the identity store for the configured backend. Returns the
/// combined `IdentityAndTeamStore` trait object so call sites can
/// extract both `IdentityStore` and `TeamStore` views without holding
/// two separate handles. Falls back to SQLite (with a warning) when
/// Postgres is requested but the pool is unavailable, so
/// misconfigured Postgres never fails hub startup.
async fn build_identity_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_identity::store::IdentityAndTeamStore>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_store_backend, StoreBackend};
    use workforce_identity::sqlite::SqliteIdentityStore;

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    if resolve_store_backend(
        "identity",
        wf_config.identity.backend.as_str(),
        pg.is_some(),
    )? == StoreBackend::Postgres
    {
        let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
        tracing::info!("Using Postgres identity store");
        let store = workforce_identity::postgres::PostgresIdentityStore::new(pg_cfg)
            .await
            .context("constructing PostgresIdentityStore")?;
        return Ok(store as Arc<dyn workforce_identity::store::IdentityAndTeamStore>);
    }

    let identity_db_path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("identities.db");
    tracing::info!(path = %identity_db_path.display(), "Using SQLite identity store");
    workforce_common::private_fs::ensure_private_sqlite(&identity_db_path)?;
    let store = SqliteIdentityStore::new(&identity_db_path).with_context(|| {
        format!(
            "opening SQLite identity store at {}",
            identity_db_path.display()
        )
    })?;
    Ok(Arc::new(store))
}

/// Build the permission cascade store for the configured backend.
/// Same SQLite-default-with-Postgres-fallback semantics as the
/// identity / pairing / project stores. Returns `Ok(None)` when
/// `[permissions] enabled = false` so callers can skip wiring the
/// evaluator without treating disabled as an error.
async fn build_permission_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Option<Arc<dyn workforce_permissions::store::PermissionStore>>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_optional_store_backend, OptionalStoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    match resolve_optional_store_backend(
        "permissions",
        wf_config.permissions.enabled,
        wf_config.permissions.backend.as_str(),
        pg.is_some(),
    )? {
        OptionalStoreBackend::Disabled => Ok(None),
        OptionalStoreBackend::Postgres => {
            let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
            tracing::info!("Using Postgres permission store");
            let store = workforce_permissions::postgres::PostgresPermissionStore::new(pg_cfg)
                .await
                .context("constructing PostgresPermissionStore")?;
            Ok(Some(
                store as Arc<dyn workforce_permissions::store::PermissionStore>,
            ))
        }
        OptionalStoreBackend::Sqlite => {
            let path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("permissions.db");
            tracing::info!(path = %path.display(), "Using SQLite permission store");
            workforce_common::private_fs::ensure_private_sqlite(&path)?;
            let store = workforce_permissions::sqlite::SqlitePermissionStore::open(&path)
                .with_context(|| {
                    format!("opening SQLite permission store at {}", path.display())
                })?;
            Ok(Some(Arc::new(store)))
        }
    }
}

/// Build the capability-override store backing the admin-editable matrix.
/// Unlike [`build_permission_store`] this is **not** gated on `[permissions]
/// enabled`: the capability matrix is core authorization, always active. It
/// shares the same backend + `permissions.db` / `permissions` schema (the
/// concrete stores implement both traits), so the override tables live
/// alongside the rule tables.
async fn build_capability_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_permissions::CapabilityStore>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_store_backend, StoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    if resolve_store_backend(
        "permissions",
        wf_config.permissions.backend.as_str(),
        pg.is_some(),
    )? == StoreBackend::Postgres
    {
        let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
        tracing::info!("Using Postgres capability store");
        let store = workforce_permissions::postgres::PostgresPermissionStore::new(pg_cfg)
            .await
            .context("constructing PostgresPermissionStore for the capability store")?;
        return Ok(store as Arc<dyn workforce_permissions::CapabilityStore>);
    }

    let path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("permissions.db");
    tracing::info!(path = %path.display(), "Using SQLite capability store");
    workforce_common::private_fs::ensure_private_sqlite(&path)?;
    let store = workforce_permissions::sqlite::SqlitePermissionStore::open(&path)
        .with_context(|| format!("opening SQLite capability store at {}", path.display()))?;
    Ok(Arc::new(store))
}

/// Build the durable pending-approval store — gated by `[approvals]
/// enabled`. Mirrors [`build_permission_store`]: Postgres when configured,
/// else a SQLite file under the hub data dir. `None` when disabled.
/// Build the runtime budget-override store: Postgres when `[postgres]` is
/// enabled. Installed by the `serve` command only — one-shot commands (`run`,
/// `cron`) intentionally run without runtime overrides.
/// enabled (shared across replicas — an override granted anywhere applies
/// fleet-wide), else SQLite at `{data_dir}/budget_overrides.db`. Always on:
/// it is inert until a SysAdmin writes to it.
async fn build_budget_override_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_hub::budget_overrides::BudgetOverrideStore>> {
    use eyre::WrapErr;
    if let Some(pg_cfg) = wf_config.postgres.as_ref().filter(|p| p.enabled) {
        let store =
            workforce_hub::budget_overrides::postgres::PostgresBudgetOverrideStore::new(pg_cfg)
                .await
                .context("initialising Postgres budget override store")?;
        return Ok(store);
    }
    let path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("budget_overrides.db");
    workforce_common::private_fs::ensure_private_sqlite(&path)?;
    let store = workforce_hub::budget_overrides::sqlite::SqliteBudgetOverrideStore::open(&path)
        .context("initialising SQLite budget override store")?;
    Ok(Arc::new(store))
}

async fn build_pending_approval_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Option<Arc<dyn workforce_hub::approvals::PendingApprovalStore>>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_optional_store_backend, OptionalStoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    match resolve_optional_store_backend(
        "approvals",
        wf_config.approvals.enabled,
        wf_config.approvals.backend.as_str(),
        pg.is_some(),
    )? {
        OptionalStoreBackend::Disabled => Ok(None),
        OptionalStoreBackend::Postgres => {
            let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
            tracing::info!("Using Postgres pending-approval store");
            let store = workforce_hub::approvals::PostgresPendingApprovalStore::new(pg_cfg)
                .await
                .context("constructing PostgresPendingApprovalStore")?;
            Ok(Some(
                store as Arc<dyn workforce_hub::approvals::PendingApprovalStore>,
            ))
        }
        OptionalStoreBackend::Sqlite => {
            let path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("approvals.db");
            tracing::info!(path = %path.display(), "Using SQLite pending-approval store");
            workforce_common::private_fs::ensure_private_sqlite(&path)?;
            let store = workforce_hub::approvals::SqlitePendingApprovalStore::open(&path)
                .with_context(|| {
                    format!(
                        "opening SQLite pending-approval store at {}",
                        path.display()
                    )
                })?;
            Ok(Some(Arc::new(store)))
        }
    }
}

/// Build the pairing store for the configured backend. Same fallback
/// semantics as `build_identity_store`.
async fn build_pairing_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_pairing::PairingStore>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_store_backend, StoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    let store: Arc<dyn workforce_pairing::PairingStore> = if resolve_store_backend(
        "pairing_store",
        wf_config.pairing_store.backend.as_str(),
        pg.is_some(),
    )? == StoreBackend::Postgres
    {
        let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
        tracing::info!("Using Postgres pairing store");
        workforce_pairing::PostgresPairingStore::new(pg_cfg)
            .await
            .context("constructing PostgresPairingStore")?
            as Arc<dyn workforce_pairing::PairingStore>
    } else {
        let pairing_db_path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("pairing.db");
        tracing::info!(path = %pairing_db_path.display(), "Using SQLite pairing store");
        workforce_common::private_fs::ensure_private_sqlite(&pairing_db_path)?;
        Arc::new(workforce_pairing::SqlitePairingStore::open(
            &pairing_db_path,
        )?)
    };

    // Device tokens are server-authoritative: heal any device whose stored
    // scopes drift from the canonical surface (legacy `["*"]` backfills down,
    // pre-enforcement partial grants up) without forcing a re-pair.
    let healed = store.normalize_device_scopes(&workforce_hub::jwt::device_scopes())?;
    if healed > 0 {
        tracing::info!(
            devices = healed,
            "Re-scoped device tokens to the canonical surface"
        );
    }
    Ok(store)
}

/// Build the per-agent project store for the configured backend. Same
/// fallback semantics as `build_identity_store`.
async fn build_project_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_projects::ProjectStore>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_store_backend, StoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    if resolve_store_backend(
        "projects",
        wf_config.projects.backend.as_str(),
        pg.is_some(),
    )? == StoreBackend::Postgres
    {
        let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
        tracing::info!("Using Postgres project store");
        let store = workforce_projects::postgres::PostgresProjectStore::new(pg_cfg)
            .await
            .context("constructing PostgresProjectStore")?;
        return Ok(store as Arc<dyn workforce_projects::ProjectStore>);
    }

    let project_db_path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("projects.db");
    tracing::info!(path = %project_db_path.display(), "Using SQLite project store");
    workforce_common::private_fs::ensure_private_sqlite(&project_db_path)?;
    Ok(Arc::new(
        workforce_projects::sqlite::SqliteProjectStore::new(&project_db_path).with_context(
            || {
                format!(
                    "opening SQLite project store at {}",
                    project_db_path.display()
                )
            },
        )?,
    ))
}

/// Build the per-agent MCP server install store for the configured
/// backend. Same SQLite-default-with-Postgres-fallback semantics as
/// `build_project_store` / `build_pairing_store`.
///
/// Called from the hub `Run` command alongside `build_project_store`;
/// the result is held for the `McpServerManager` (PR5) which uses it
/// to hydrate live `McpServerRegistry` state at startup and to
/// persist agent-driven / UI-driven installs without restarts.
async fn build_mcp_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_mcp_store::McpServerStore>> {
    use eyre::WrapErr;
    use workforce_common::backend::{resolve_store_backend, StoreBackend};

    let pg = wf_config.postgres.as_ref().filter(|p| p.enabled);
    if resolve_store_backend(
        "mcp_store",
        wf_config.mcp_store.backend.as_str(),
        pg.is_some(),
    )? == StoreBackend::Postgres
    {
        let pg_cfg = pg.expect("postgres backend implies an enabled [postgres] pool");
        tracing::info!("Using Postgres mcp-store");
        let store = workforce_mcp_store::postgres::PostgresMcpServerStore::new(pg_cfg)
            .await
            .context("constructing PostgresMcpServerStore")?;
        return Ok(store as Arc<dyn workforce_mcp_store::McpServerStore>);
    }

    let mcp_db_path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("mcp_store.db");
    tracing::info!(path = %mcp_db_path.display(), "Using SQLite mcp-store");
    workforce_common::private_fs::ensure_private_sqlite(&mcp_db_path)?;
    Ok(Arc::new(
        workforce_mcp_store::sqlite::SqliteMcpServerStore::new(&mcp_db_path)
            .with_context(|| format!("opening SQLite mcp-store at {}", mcp_db_path.display()))?,
    ))
}

/// Build the OAuth pending-redirect state store. Selects the
/// in-memory impl by default (matching the original Google adapter)
/// or a SQLite-backed impl when the operator opts in via
/// `[oauth_state] backend = "sqlite"`. The persistent variant lets
/// a UI-initiated MCP-install OAuth flow survive a hub restart
/// between authorise-click and vendor-callback.
/// Newtype around `HashMap<agent_id, McpServerManager>` so the
/// hub-internal callback handler can resolve "which manager owns
/// this OAuth state" without coupling the hub crate to a specific
/// container type. Held as `Arc<McpManagerMap>` and passed into the
/// callback route's `ManagerLookup` slot.
/// Interior mutability so the config-overlay watcher can register managers
/// for live-spawned agents.
struct McpManagerMap(
    parking_lot::RwLock<
        std::collections::HashMap<String, Arc<workforce_hub::mcp_servers::McpServerManager>>,
    >,
);

impl McpManagerMap {
    fn insert(&self, agent_id: String, manager: Arc<workforce_hub::mcp_servers::McpServerManager>) {
        self.0.write().insert(agent_id, manager);
    }

    fn remove(&self, agent_id: &str) {
        self.0.write().remove(agent_id);
    }
}

impl workforce_hub::mcp_servers::callback::ManagerLookup for McpManagerMap {
    fn manager_for(
        &self,
        agent_id: &str,
    ) -> Option<Arc<workforce_hub::mcp_servers::McpServerManager>> {
        self.0.read().get(agent_id).cloned()
    }
}

/// Everything the config-overlay watcher needs to build and wire an agent at
/// runtime — mirrors the startup wiring (workflow runtime injection, advisor,
/// coding.session, MCP manager, image broadcast, NATS chat) exactly.
struct OverlayWatcherCtx {
    registry: Arc<workforce_hub::agent_registry::AgentRegistry>,
    config_path: PathBuf,
    data_dir: PathBuf,
    lock_manager: Arc<workforce_common::locking::LockManager>,
    shared_knowledge: Option<Arc<workforce_hub::knowledge::KnowledgeService>>,
    shared_local_agents: Option<Arc<workforce_hub::local_agent_registry::LocalAgentRegistry>>,
    global_vault: Option<Arc<workforce_hub::vault::Vault>>,
    subscriptions: workforce_hub::subscription::Subscriptions,
    engagement_overrides: Arc<workforce_common::engagement::EngagementOverrides>,
    project_store: Arc<dyn workforce_projects::ProjectStore>,
    mcp_store: Arc<dyn workforce_mcp_store::McpServerStore>,
    oauth_state: Arc<dyn workforce_oauth_client::OAuthStateStore>,
    mcp_http: Arc<reqwest::Client>,
    mcp_managers: Arc<McpManagerMap>,
    broadcast_tx: tokio::sync::broadcast::Sender<workforce_hub::gateway::BroadcastMessage>,
    /// Shared reminder-delivery callback and leader flag, so a live-spawned
    /// agent gets its own scheduler (and reminder/scheduler tools) exactly like
    /// a startup agent, rather than `scheduler = None` until the next restart.
    scheduler_delivery: workforce_hub::scheduler::DeliveryCallback,
    voice_announcer: Arc<workforce_hub::voice_announce::VoiceAnnouncer>,
    leader_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    nats: Option<async_nats::Client>,
    /// Identity store + constitutional capability gate, late-bound: both are
    /// built *after* this ctx is constructed (the identity store at boot, after
    /// the watcher spawns), so they're shared as cells the boot path fills in
    /// and `spawn_and_wire_agent` reads. Without this, a live-spawned agent
    /// would have neither the inline admin store (`set_identity_store`) nor the
    /// pre-dispatch gate — reopening the admin hole this change closes.
    identity_store:
        Arc<parking_lot::RwLock<Option<Arc<dyn workforce_identity::store::IdentityAndTeamStore>>>>,
    capability_gate:
        Arc<parking_lot::RwLock<Option<Arc<dyn workforce_agent::runner::CapabilityGate>>>>,
    /// The Slack integration, late-bound the same way: it starts after the
    /// watcher, and a live-spawned agent is wired into it like a boot one.
    slack: workforce_hub::slack_onboarding::SlackSlot,
    /// Woken by the gateway after an overlay write so agent create/delete
    /// reconcile in build-time rather than the poll interval.
    kick: Arc<tokio::sync::Notify>,
}

/// Live agent spawn/teardown. Polls the overlay file's mtime; on change, reloads the
/// merged config and reconciles the live registry:
///
/// * **new agent** → `build_for_agent` + full wiring, live without a restart
/// * **removed agent** → dropped from the registry (in-flight sessions keep
///   their `Arc` until they finish; new lookups stop resolving immediately)
/// * **changed agent** → respawned (teardown + build) with the new entry
/// * the **default agent** is never reconciled live — restart required
///
/// Non-agent overlay sections (llm.routing etc.) still need a restart; the
/// watcher only reconciles the agent set.
fn spawn_overlay_agent_watcher(ctx: OverlayWatcherCtx) {
    use workforce_common::settings::WorkforceConfig;
    tokio::spawn(async move {
        let overlay_path = ctx
            .data_dir
            .join(workforce_common::config_overlay::OVERLAY_FILE);
        // Change detection on file CONTENT, not mtime — the overlay is small,
        // and content comparison can't miss same-timestamp rewrites.
        let read_overlay = |p: &std::path::Path| std::fs::read_to_string(p).ok();
        let mut last_seen = read_overlay(&overlay_path);
        // Each live agent's config shape, for changed-entry detection.
        let mut known: std::collections::HashMap<String, serde_json::Value> =
            match WorkforceConfig::load(&ctx.config_path) {
                Ok(c) => c
                    .agents
                    .iter()
                    .map(|a| (a.id.clone(), serde_json::to_value(a).unwrap_or_default()))
                    .collect(),
                Err(_) => Default::default(),
            };
        tracing::info!(overlay = %overlay_path.display(), "Config overlay watcher started (live agent spawn)");

        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                _ = ctx.kick.notified() => {}
            }
            let seen = read_overlay(&overlay_path);
            if seen == last_seen {
                continue;
            }
            last_seen = seen;

            let new_config = match WorkforceConfig::load(&ctx.config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "Overlay changed but merged config failed to load — keeping current agents");
                    continue;
                }
            };
            let default_id = ctx.registry.default_id().to_string();
            let desired: std::collections::HashMap<
                String,
                workforce_common::settings::AgentConfig,
            > = new_config
                .agents
                .iter()
                .map(|a| (a.id.clone(), a.clone()))
                .collect();
            let current: Vec<String> = ctx
                .registry
                .agent_ids()
                .iter()
                .map(|i| i.as_str().to_string())
                .collect();

            ctx.registry.retain_unloaded(|id| desired.contains_key(id));

            // Teardown removed agents.
            for id in &current {
                if desired.contains_key(id) {
                    continue;
                }
                if *id == default_id {
                    tracing::warn!(agent_id = %id, "Default agent removed from config — restart required");
                    continue;
                }
                ctx.registry.remove(id);
                ctx.mcp_managers.remove(id);
                known.remove(id);
                if let Some(slack) = ctx.slack.get() {
                    slack.agent_left(id).await;
                }
            }

            // Spawn new agents / respawn changed ones.
            for (id, agent_config) in &desired {
                let shape = serde_json::to_value(agent_config).unwrap_or_default();
                let is_new = !current.contains(id);
                let changed = !is_new && known.get(id) != Some(&shape);
                if !is_new && !changed {
                    continue;
                }
                if *id == default_id {
                    if changed {
                        tracing::warn!(agent_id = %id, "Default agent config changed — restart required to apply");
                        known.insert(id.clone(), shape);
                    }
                    continue;
                }
                ctx.registry.begin_loading(agent_config);
                if changed {
                    ctx.registry.remove(id);
                    ctx.mcp_managers.remove(id);
                }
                match workforce_hub::agent_registry::loaded(
                    id,
                    spawn_and_wire_agent(&ctx, &new_config, agent_config, is_new).await,
                ) {
                    Ok(()) => {
                        known.insert(id.clone(), shape);
                        tracing::info!(
                            agent_id = %id,
                            respawn = changed,
                            "Agent live-loaded from config overlay (no restart)"
                        );
                    }
                    Err(reason) => ctx.registry.load_failed(agent_config, reason),
                }
            }
        }
    });
}

/// Build one agent service and apply the same wiring the startup path does.
async fn spawn_and_wire_agent(
    ctx: &OverlayWatcherCtx,
    config: &workforce_common::settings::WorkforceConfig,
    agent_config: &workforce_common::settings::AgentConfig,
    spawn_chat_worker: bool,
) -> eyre::Result<()> {
    // Build with the agent's own scheduler (reminder/scheduler tools +
    // consolidation job), matching the startup path, then run its tick loop.
    let (service, agent_scheduler) =
        workforce_hub::serve::AgentService::build_for_agent_with_scheduler(
            config.clone(),
            None, // config_path not available in hub server mode (matches startup)
            agent_config,
            Arc::clone(&ctx.scheduler_delivery),
            Arc::clone(&ctx.lock_manager),
            ctx.shared_knowledge.clone(),
            ctx.shared_local_agents.clone(),
            ctx.registry
                .default_agent()
                .and_then(|s| s.workflow_runtime().cloned()),
            ctx.global_vault.clone(),
            ctx.subscriptions.clone(),
        )
        .await?;
    if let Some(ref flag) = ctx.leader_flag {
        agent_scheduler.set_leader_flag(Arc::clone(flag));
        service.set_leader_flag(Arc::clone(flag));
    }
    agent_scheduler.set_voice_announcer(Arc::clone(&ctx.voice_announcer));
    // Only reached for non-default agents (a default-agent change requires a
    // restart), and the workflow runtime arms WorkflowStart jobs on the
    // default's scheduler only — anything this store reloaded is stale.
    let swept = agent_scheduler.remove_workflow_start_jobs().await;
    if swept > 0 {
        tracing::info!(
            agent_id = %agent_config.id,
            removed = swept,
            "Swept stale workflow schedule jobs from non-owner scheduler"
        );
    }
    // Safe to go ready immediately: the live registry is already wired into
    // the shared workflow runtime at this point in the process lifetime.
    agent_scheduler.set_ready();
    // The tick loop is driven by `supervise_scheduler` once the service is in
    // the registry (below); a respawn retires the stale scheduler on removal,
    // and the supervisor's reconcile reaps its loop.

    if let Some(wf_runtime) = service.workflow_runtime() {
        wf_runtime.set_agent_registry(
            Arc::clone(&ctx.registry) as Arc<dyn workforce_hub::hosts::RegistryView>
        );
        if let Some(patterns) = config.mcp.step_patterns() {
            wf_runtime.set_mcp_exposed_tools(patterns);
        }
    }

    let id = agent_config.id.clone();
    ctx.registry.wire_advisor_tools_for(&service);
    workforce_hub::engagement_tool::register_engagement_tools(
        &service,
        &ctx.engagement_overrides,
        ctx.global_vault.clone(),
    );
    workforce_hub::desk_driver::register_tools(&service);
    workforce_hub::agent_registry::AgentRegistry::wire_coding_session_tools_for(
        &service,
        Arc::clone(&ctx.project_store),
        workforce_hub::projects::layout::WorkRoots::from_hub(&config.hub),
        config.mcp.enabled,
        config.coding_session.clone(),
        config.exec.clone(),
        &ctx.subscriptions,
    );

    // MCP server manager — mirrors the startup per-service block.
    let manager = workforce_hub::mcp_servers::McpServerManager::new(
        service.agent_id.as_str().to_string(),
        Arc::clone(&ctx.mcp_store),
        Arc::clone(&service.tools),
        service.vault.clone(),
        service.global_vault.clone(),
        Arc::clone(&ctx.oauth_state),
        Some(Arc::clone(&service.cost_tracker)),
        Arc::clone(&ctx.mcp_http),
    );
    if let Err(e) = manager.hydrate_from_store().await {
        tracing::warn!(agent = %id, error = %e, "MCP hydrate-from-store failed at live spawn");
    }
    if let Err(e) = manager.import_from_toml(&config.mcp).await {
        tracing::warn!(agent = %id, error = %e, "MCP TOML import failed at live spawn");
    }
    workforce_hub::mcp_servers::register_manager_tools(
        Arc::clone(&manager),
        Arc::clone(&service.tools),
        config.hub.public_url.clone(),
    );
    let access = Arc::new(workforce_hub::mcp_servers::McpToolAccess::new(
        manager.registry(),
        Arc::clone(&service.tools),
        service.vault.clone(),
        service.global_vault.clone(),
    ));
    service.set_tool_access(workforce_hub::wasm_skill_access::compose_tool_access(
        &service, access,
    ));
    service.set_mcp_manager(Arc::clone(&manager));
    service.set_image_broadcast(ctx.broadcast_tx.clone());
    if let Some(store) = workforce_hub::serve::shared_workflow_store(config).await {
        if let Err(e) = workforce_hub::environments::adopt_legacy_environments(
            &store,
            &config.environments,
            &service,
        )
        .await
        {
            tracing::error!(agent = %id, error = %e, "legacy environments not moved; this agent runs without them");
        }
    }

    // Admin gating for live-spawned agents — without this they'd have neither
    // the inline-check identity store nor the pre-dispatch capability gate, so
    // the privileged agent.*/role.*/skill/credential tools would be ungated.
    // Both are late-bound cells the boot path fills (the identity store is
    // built after this watcher spawns), so a live spawn that races boot simply
    // wires nothing here and the next reconcile picks it up.
    if let Some(store) = ctx.identity_store.read().clone() {
        service.set_identity_store(store.clone().as_identity_store());
        service.set_team_store(store.clone().as_team_store());
        service.set_remote_mcp_token_store(store.as_remote_mcp_token_store());
    }
    if let Some(gate) = ctx.capability_gate.read().clone() {
        service.set_capability_gate(gate);
    } else {
        tracing::warn!(
            agent = %id,
            "live-spawned agent has no capability gate yet (boot wiring not complete); \
             admin tools rely on the inline store check until the next reconcile"
        );
    }

    ctx.mcp_managers.insert(id.clone(), manager);

    if let Some(nats) = &ctx.nats {
        service.set_nats(nats.clone());
    }

    let slack_wired = match ctx.slack.get() {
        Some(slack) => {
            slack.wire_agent(&service).await;
            true
        }
        None => false,
    };

    // Registry insertion comes LAST: the service only becomes resolvable
    // once fully wired, and a failure anywhere above leaves no partial
    // agent behind (it is retried on the next overlay change).
    ctx.registry.insert(&id, Arc::clone(&service));
    // Slack started between the two reads: wire it now. Started after both,
    // and boot's own pass over the registry finds this agent.
    if let Some(slack) = ctx.slack.get() {
        if !slack_wired {
            slack.wire_agent(&service).await;
        }
        slack.agent_joined(&id).await;
    }

    // NATS chat worker (new agents only — workers resolve the service from
    // the registry per message, so a respawn reuses the existing worker).
    if spawn_chat_worker {
        if let Some(nats) = &ctx.nats {
            workforce_hub::chat_worker::spawn_chat_worker(
                nats.clone(),
                Arc::clone(&ctx.registry),
                AgentId::new(&id),
            );
        }
    }
    Ok(())
}

/// Background task: poll the vector-backend health probe every 30s
/// and emit one structured log line per state transition. Stops when
/// `drain_token` is cancelled.
fn spawn_vector_backend_health_logger(
    probe: Arc<workforce_hub::knowledge::BackendHealthProbe>,
    drain: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut previous: Option<workforce_hub::knowledge::VectorBackendHealth> = None;
        let tick = std::time::Duration::from_secs(30);
        loop {
            tokio::select! {
                _ = drain.cancelled() => break,
                _ = tokio::time::sleep(tick) => {
                    let current = probe.current().await;
                    workforce_hub::knowledge::log_vector_health_transition(
                        previous.as_ref(),
                        &current,
                    );
                    previous = Some(current);
                }
            }
        }
    });
}

fn build_oauth_state_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_oauth_client::OAuthStateStore>> {
    use eyre::WrapErr;
    match wf_config.oauth_state.backend.as_str() {
        "sqlite" => {
            let path = std::path::PathBuf::from(&wf_config.hub.data_dir).join("oauth_state.db");
            tracing::info!(path = %path.display(), "Using SQLite OAuth state store");
            workforce_common::private_fs::ensure_private_sqlite(&path)?;
            let store =
                workforce_oauth_client::SqliteOAuthStateStore::new(&path).with_context(|| {
                    format!("opening SQLite OAuth state store at {}", path.display())
                })?;
            Ok(Arc::new(store))
        }
        _ => {
            tracing::info!("Using in-memory OAuth state store");
            Ok(Arc::new(
                workforce_oauth_client::InMemoryOAuthStateStore::new(),
            ))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Before any TLS client exists, including the telemetry exporters
    // `setup_logging` builds.
    workforce_common::tls::install_default_crypto_provider();
    workforce_common::errors::setup_error_handling()?;
    // `mcp-serve` speaks JSON-RPC on stdout; the global subscriber writes
    // logs to stdout (DD JSON formatter) and pollutes the protocol stream —
    // its own arm installs a stderr subscriber instead. The arm's
    // `try_init()` silently loses if this global one runs first, which is
    // exactly how prod mcp-serve ended up emitting OTel JSON between
    // JSON-RPC frames.
    // `decide` writes examples, records and picks to stdout, and what it
    // reads holds what people asked: its logs go to stderr too.
    let command = std::env::args().nth(1);
    let is_mcp_serve = command.as_deref() == Some("mcp-serve");
    if command.as_deref() == Some("decide") {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .try_init();
    } else if !is_mcp_serve {
        workforce_common::logging::setup_logging()?;
    }

    {
        // Fan-out: every audit event lands in both the structured
        // tracing pipeline (operator dashboards) and the in-process
        // ring buffer (admin UI's /api/permissions/audit endpoint).
        let ring = workforce_memory::access_audit::install_ring_buffer(5_000);
        let tracing_sink: std::sync::Arc<dyn workforce_memory::access_audit::AccessAuditSink> =
            std::sync::Arc::new(workforce_memory::access_audit::TracingAuditSink);
        let ring_sink: std::sync::Arc<dyn workforce_memory::access_audit::AccessAuditSink> = ring;
        let fan_out =
            std::sync::Arc::new(workforce_memory::access_audit::FanOutAuditSink::new(vec![
                tracing_sink,
                ring_sink,
            ]));
        let _ = workforce_memory::access_audit::set_global_sink(fan_out);
    }

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            task,
            agent,

            max_rounds,
        } => {
            let (provider_kind, api_key) = resolve_provider_and_key(&cli.provider, cli.api_key)?;

            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());

            eprintln!(
                "{} Running task with agent '{}' via {:?} in {}",
                style("🚀").bold(),
                style(&agent).cyan(),
                provider_kind,
                style(workdir.display()).dim()
            );

            // Try to load config for failover/Weaviate support
            let wf_config =
                workforce_common::settings::WorkforceConfig::load(&workdir.join("workforce.toml"))
                    .ok();

            let thinking_budget = wf_config
                .as_ref()
                .and_then(|c| c.llm.thinking_budget_tokens);
            let zai_sub = wf_config.as_ref().is_some_and(|c| c.llm.zai_subscription);
            let base_llm =
                create_provider_async(provider_kind, &api_key, None, thinking_budget, zai_sub)
                    .await?;
            let llm = wrap_with_failover(base_llm, wf_config.as_ref());

            // Set up tools (sandboxed to workdir)
            let tools = default_tools(vec![workdir.clone()], ToolSecrets::from_env());

            let agent_id = workforce_core::types::AgentId::new(&agent);
            let memory_store: Option<Arc<dyn workforce_memory::store::MemoryStore>> =
                if let Some(ref cfg) = wf_config {
                    // A backend no store can be built for is refused, not
                    // run without memory.
                    cfg.check_memory_backend()?;
                    if cfg.weaviate.enabled || cfg.memory.is_vellev() {
                        let store = workforce_hub::embeddings::switched_memory_store(
                            cfg,
                            workforce_hub::embeddings::base_store(cfg)?,
                        )
                        .await?;
                        match store.ensure_collection().await {
                            Ok(()) => {
                                eprintln!("  {} Weaviate memory connected", style("🧠").bold());
                                let store: Arc<dyn workforce_memory::store::MemoryStore> = store;
                                Some(store)
                            }
                            Err(_) => None,
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

            if let Some(ref store) = memory_store {
                tools.register(Arc::new(workforce_hub::memory_tool::MemoryTool::new(
                    Arc::clone(store),
                    agent_id,
                    workdir.clone(),
                )));
            }

            // Set up policy engine
            let policy = if let Some(ref policy_path) = cli.policy {
                let path_str = policy_path.to_str().unwrap_or("");
                PolicyEngine::from_yaml(
                    &std::fs::read_to_string(path_str)
                        .map_err(|e| eyre::eyre!("Failed to read policy file: {}", e))?,
                )?
            } else {
                // Default: allow all tools
                PolicyEngine::from_yaml(
                    r#"
rules:
  - name: "allow-all"
    scope: global
    tool: "*"
    action: allow
"#,
                )?
            };

            // Load identity from workspace (SOUL.md, AGENTS.md, daily notes, active task)
            let mut identity = AgentIdentity::load(&workdir)?;
            let system_prompt = identity.system_prompt();

            let config = RunnerConfig {
                agent_id: AgentId::new(&agent),
                system_prompt,

                model: None,
                max_tokens: Some(8192),
                max_rounds,
                team_id: None,
                role: None,
                compaction: workforce_common::session::CompactionConfig::default(),
                spill: workforce_common::session::SpillConfig::default(),
                routing: workforce_agent::model_router::RoutingConfig::default(),
                tier_registry: workforce_agent::llm::tiers::TierRegistry::new(None),
                model_max_tokens: std::collections::HashMap::new(),
                context_eviction_interval: None,
                context_eviction_keep_last: 3,
                trace_writer: None,
                trace_tags: Default::default(),
                permission_writer: None,
                permission_gate: None,
                arguments: workforce_agent::tools::arguments::ArgumentValidation::default(),
                tool_loading: workforce_agent::tools::loading::ToolLoading::default(),
                programs: None,
                // CLI single-run is a trusted operator context (no per-user
                // identity in scope), so the constitutional gate is unwired.
                capability_gate: None,
                approval_router: None,
                preexecute: None,
                journal: None,
                collect: Vec::new(),
                seeds: None,
                interactive: false,
                self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
                compile_gate: None,
            };

            let runner =
                AgentRunner::new(config, Arc::from(llm), Arc::new(tools), Arc::new(policy));

            let result = runner.run(&task).await?;

            // Print results
            println!("\n{}", result.response);

            eprintln!(
                "\n{} {} rounds, {} tool calls, {} in / {} out tokens",
                style("📊").bold(),
                result.rounds,
                result.tool_invocations.len(),
                result.total_usage.input_tokens,
                result.total_usage.output_tokens,
            );

            if !result.tool_invocations.is_empty() {
                eprintln!("{}", style("Tools used:").dim());
                for inv in &result.tool_invocations {
                    let status = if inv.success {
                        style("✓").green()
                    } else {
                        style("✗").red()
                    };
                    eprintln!(
                        "  {} {} ({}ms, policy: {})",
                        status, inv.tool_name, inv.duration_ms, inv.policy_decision
                    );
                }
            }
        }
        Commands::Chat {
            agent,

            gateway,
            token,
        } => {
            if !workforce_gateway_client::gateway_available(&gateway).await {
                return Err(eyre::eyre!("Gateway not available at {}", gateway));
            }

            let wf_config = workforce_common::settings::WorkforceConfig::load(
                &cli.workdir.join("workforce.toml"),
            )
            .ok();
            let model_name = wf_config
                .as_ref()
                .map(|c| c.llm.default_model.as_str())
                .unwrap_or("unknown");
            let agent_arg = if agent == "default" {
                None
            } else {
                Some(agent.as_str())
            };
            tui::run(&gateway, &token, model_name, agent_arg).await?
        }
        Commands::Serve { config } => {
            use tokio::sync::broadcast;
            use workforce_common::settings::WorkforceConfig;

            let config_path = config.clone();
            let wf_config = if config.exists() {
                WorkforceConfig::load(&config)?
            } else {
                eprintln!(
                    "{} Config file '{}' not found, using defaults",
                    style("⚠️").yellow(),
                    config.display()
                );
                WorkforceConfig::default()
            };

            eprintln!("{} Starting Workforce Agent Service", style("🚀").bold(),);
            eprintln!(
                "  Provider: {}, Model: {}",
                style(&wf_config.llm.provider).cyan(),
                style(&wf_config.llm.default_model).cyan(),
            );

            // Before anything can dispatch a command as the agent uid.
            workforce_hub::private_state::close_hub_state(&wf_config);

            // Never serve the artifact index from vellev while a shadow of
            // it is kept or not promoted (DW-676). Memories are guarded
            // where they are opened.
            guard_before_serving(&wf_config).await?;

            // Install the process-wide secret cache before any agent is
            // built. The cache backs `tokenize_secrets` at every ingress
            // surface and the substitution layer at the secret-sink tool
            // boundary. Defaults: 60-minute sliding TTL, 16 markers per
            // session. Background sweep runs every 5 minutes.
            {
                let cache = std::sync::Arc::new(
                    workforce_hub::secret_cache::InMemorySecretCache::with_defaults(),
                );
                cache.spawn_gc_task(std::time::Duration::from_secs(5 * 60));
                workforce_common::secret_marker::set_global_cache(cache);
                eprintln!("  Secret cache: {} (in-memory)", style("enabled").cyan(),);
            }

            // Install the runtime budget-override store before any agent is
            // built — each service's CostGovernor picks it up at construction.
            {
                let store = build_budget_override_store(&wf_config).await?;
                let backend = if wf_config.postgres.as_ref().is_some_and(|p| p.enabled) {
                    "postgres"
                } else {
                    "sqlite"
                };
                workforce_hub::budget_overrides::set_global_store(store);
                eprintln!(
                    "  Budget overrides: {} ({backend})",
                    style("enabled").cyan()
                );
            }

            // Install the Metabase schema store before any agent is built: the
            // skill loader reads it when constructing each agent's clients, and
            // a client built before it lands would never see the schema.
            if let Some(pg_cfg) = wf_config.postgres.as_ref().filter(|p| p.enabled) {
                match workforce_common::metabase_schema_postgres::MetabaseSchemaPostgresStore::new(
                    pg_cfg,
                )
                .await
                {
                    Ok(store) => {
                        workforce_common::metabase_schema::set_store(store);
                        eprintln!("  Metabase schema: {} (postgres)", style("enabled").cyan());
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "Metabase schema store unavailable; the native-query tool schema \
                         will carry only what each turn learns"
                    ),
                }
            }

            // Create broadcast channel first (needed by scheduler delivery and reactions)
            let (broadcast_tx, _) =
                broadcast::channel::<workforce_hub::gateway::BroadcastMessage>(256);
            let broadcast_tx_for_scheduler = broadcast_tx.clone();
            // One delivery callback shared by every agent's scheduler — it just
            // broadcasts a reminder to connected WebSocket clients.
            let scheduler_delivery: workforce_hub::scheduler::DeliveryCallback = Arc::new(
                move |job_name: String, response: String| {
                    let tx = broadcast_tx_for_scheduler.clone();
                    Box::pin(async move {
                        tracing::info!(
                            job = %job_name,
                            receivers = tx.receiver_count(),
                            "Delivering reminder via broadcast"
                        );
                        match tx.send(workforce_hub::gateway::BroadcastMessage::new(
                            "scheduler",
                            "reminder",
                            format!("⏰ **{}**: {}", job_name, response),
                        )) {
                            Ok(n) => tracing::info!(receivers = n, "Reminder broadcast sent"),
                            Err(e) => {
                                tracing::debug!(error = %e, "Reminder broadcast skipped (no WebSocket receivers)")
                            }
                        }
                    })
                },
            );

            // Open the hub-level global vault BEFORE agents are built, so
            // per-user credential overlays (written to the global vault via
            // /api/me/vault/*) are layered into each per-agent skill loader.
            let global_vault = workforce_hub::vault::open_global(&wf_config).await?;

            // One hub-wide engagement-override store, created before any agent
            // so the default service, dynamic spawns, the Slack handlers, the
            // `messaging.*_mode` tools, and the admin API all share it. Persisted
            // records are per-agent (`agent_id` + channel + optional thread); a
            // legacy channel-global blob fails to parse and is intentionally
            // discarded — those unscoped entries are the bug this change fixes.
            let engagement_overrides = {
                let store = Arc::new(workforce_common::engagement::EngagementOverrides::new());
                if let Some(json) = global_vault
                    .as_ref()
                    .and_then(|v| v.get("engagement/overrides"))
                {
                    match serde_json::from_str::<Vec<workforce_common::engagement::EngagementRecord>>(
                        &json,
                    ) {
                        Ok(records) => {
                            store.load(records).await;
                            tracing::info!("Loaded engagement overrides from vault");
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            "Discarding unparseable engagement overrides from vault \
                             (legacy channel-global entries are now agent-scoped); starting fresh"
                        ),
                    }
                }
                store
            };

            // One-time migration: the scheduler job store moved from the
            // hub-level `{data_dir}/scheduler/jobs.json` to each agent's own
            // workspace. Copy the default agent's legacy file so live reminders
            // survive the upgrade (a missing legacy file — fresh install — is a
            // no-op; agent workspaces aren't symlinked, so the relative path
            // resolves to the same file the scheduler loads).
            {
                let default_ws = wf_config
                    .default_agent()
                    .map(|a| {
                        a.workspace.clone().unwrap_or_else(|| {
                            format!("{}/agents/{}", wf_config.hub.data_dir, a.id)
                        })
                    })
                    .unwrap_or_else(|| format!("{}/agents/default", wf_config.hub.data_dir));
                let legacy = std::path::Path::new(&wf_config.hub.data_dir)
                    .join("scheduler")
                    .join("jobs.json");
                let target = std::path::Path::new(&default_ws)
                    .join("scheduler")
                    .join("jobs.json");
                if legacy.exists() && !target.exists() {
                    if let Some(parent) = target.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::copy(&legacy, &target) {
                        Ok(_) => {
                            tracing::info!(from = %legacy.display(), to = %target.display(), "Migrated scheduler job store to per-agent workspace")
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to migrate scheduler job store")
                        }
                    }
                }
            }

            // The hub's subscriptions, booted once and handed to every agent.
            let subscriptions =
                workforce_hub::subscription::Subscriptions::boot(&wf_config, global_vault.as_ref());

            // Build the default agent service with scheduler and reactions
            let (default_service, scheduler) =
                workforce_hub::serve::AgentService::from_config_with_scheduler_delivery_and_reactions(
                    wf_config.clone(),
                    Some(config_path.clone()),
                    Arc::clone(&scheduler_delivery),
                    global_vault.clone(),
                    // Hub canonical model: the MaintenanceRunner is the sole
                    // indexer, so the default service must not also index its
                    // own root (which created a redundant, never-pulled index).
                    false,
                    subscriptions.clone(),
                )
                .await?;

            // Build admin API (inject scheduler into hub state)
            let mut hub_state =
                workforce_hub::state::HubState::from_config(wf_config.clone()).await?;
            let nats_for_gateway = hub_state.nats.clone();

            // Voice announce path: the per-instance registry of voice-bridge
            // connections, the announcer schedulers deliver reminders through,
            // and (in NATS mode) the relay that reaches a bridge whose socket
            // landed on another instance.
            let voice_listeners =
                Arc::new(workforce_hub::voice_announce::VoiceListeners::default());
            let voice_announcer = Arc::new(workforce_hub::voice_announce::VoiceAnnouncer::new(
                Arc::clone(&voice_listeners),
                broadcast_tx.clone(),
                nats_for_gateway.clone(),
            ));
            let _voice_relay = nats_for_gateway.clone().map(|nats| {
                workforce_hub::voice_announce::spawn_relay(
                    nats,
                    Arc::clone(&voice_listeners),
                    broadcast_tx.clone(),
                )
            });
            let session_push = workforce_hub::session_push::SessionPush::new(
                nats_for_gateway.clone(),
                broadcast_tx.clone(),
            );
            workforce_hub::session_push::install(session_push.clone());
            let _session_push_relay = nats_for_gateway
                .clone()
                .map(|nats| workforce_hub::session_push::spawn_relay(nats, broadcast_tx.clone()));

            // A declared multi-instance deployment must not boot ungated: with
            // no JetStream there is no leader election, and this instance
            // would run every singleton alongside its peers.
            if wf_config.cluster.expected && hub_state.jetstream.is_none() {
                eyre::bail!(
                    "[cluster] expected = true but NATS JetStream is unavailable — refusing to \
                     start ungated. Fix the [nats] connection, or set expected = false for a \
                     single-instance deployment."
                );
            }

            // Set up leader election if JetStream is available.
            // In multi-instance deployments, only the leader runs singleton
            // subsystems (scheduler, heartbeat, Slack) to avoid duplicates.
            // `leader_rx` notifies on leadership changes so Slack ingress can
            // open/close sockets when this instance gains/loses leadership.
            let (leader_flag, leader_rx, election_cancel): (
                Option<Arc<std::sync::atomic::AtomicBool>>,
                Option<tokio::sync::watch::Receiver<bool>>,
                Option<tokio_util::sync::CancellationToken>,
            ) = if let (Some(nats), Some(js)) = (&hub_state.nats, &hub_state.jetstream) {
                // Hostname for log correlation with the container, plus a
                // per-process suffix: two replicas sharing a hostname would
                // otherwise read each other's lease as their own.
                let instance_id = format!(
                    "{}-{}",
                    std::env::var("HOSTNAME").unwrap_or_else(|_| "hub".to_string()),
                    uuid::Uuid::new_v4()
                        .simple()
                        .to_string()
                        .get(..8)
                        .unwrap_or_default()
                );

                let election = workforce_nats::leader::LeaderElection::new(
                    nats.clone(),
                    js.clone(),
                    instance_id.clone(),
                    wf_config.nats.stream_replicas,
                );
                let flag = election.is_leader_flag();
                let rx = election.subscribe();
                // Cancelling this token makes the election loop `resign()` —
                // releasing the lease so a successor takes over in seconds
                // instead of waiting out the TTL. Cancelled from the
                // shutdown handler.
                let cancel = tokio_util::sync::CancellationToken::new();
                let cancel_for_election = cancel.clone();
                tokio::spawn(async move {
                    election.run(cancel_for_election).await;
                });
                eprintln!(
                    "  Leader:    {} (instance {})",
                    style("election active").cyan(),
                    style(instance_id.get(..8).unwrap_or(&instance_id)).yellow(),
                );
                (Some(flag), Some(rx), Some(cancel))
            } else {
                (None, None, None)
            };

            // Observability: stash the leadership flag so /api/metrics can
            // render workforce_hub_is_leader (with slack_ingress_up and
            // nats_connected) straight from process state — the fleet-health
            // gauges Datadog pages on. See hub health_gauges.rs for why these
            // bypass the metrics recorder.
            if let Some(ref flag) = leader_flag {
                workforce_hub::health_gauges::set_leader_flag(Arc::clone(flag));
            }

            // Set leader flag on scheduler so it only fires jobs when leader
            if let Some(ref flag) = leader_flag {
                scheduler.set_leader_flag(Arc::clone(flag));
            }

            scheduler.set_voice_announcer(Arc::clone(&voice_announcer));

            // Native coding tools are registered automatically in serve.rs
            eprintln!("  Native coding tools: {}", style("11 tools").cyan());

            // Register compaction management tool
            default_service.register_tool(Arc::new(
                workforce_hub::compaction_tool::CompactionTool::new(Arc::clone(&default_service)),
            ));
            eprintln!(
                "  Compaction: {} (max {}K tokens)",
                style("configurable").cyan(),
                default_service.effective_compaction().max_context_tokens / 1000
            );

            // Create event source manager for admin API (DW-428).
            // Sources (Slack workspaces, webhooks) self-register when they start.
            let event_source_manager_arc =
                Arc::new(workforce_hub::event_manager::EventSourceManager::new());

            // The default agent's scheduler tick loop is driven by
            // `supervise_scheduler` (spawned after the registry, below) along
            // with every other agent's.

            if let Some(state) = Arc::get_mut(&mut hub_state) {
                state.scheduler = Some(Arc::clone(&scheduler));
                // Share the agent service's cost tracker with the hub state
                // so the admin API dashboard sees agent runtime costs too
                state.cost_tracker = Arc::clone(&default_service.cost_tracker);
                // Set config path for CRUD persistence to workforce.toml
                state.config_path = Some(config_path.clone());
                // Share the gateway auth token for admin API write protection
                state.auth_token = wf_config
                    .gateway
                    .auth_token
                    .clone()
                    .or_else(|| std::env::var("WORKFORCE_GATEWAY_TOKEN").ok());
                // Share the default agent's job runner for the /api/subagents admin API
                state.agent_jobs = default_service.job_runner.clone();
                // Share knowledge service for admin API (DW-428)
                state.knowledge_service = default_service.knowledge_service.clone();
                // Share event source manager for admin API (DW-428)
                state.event_source_manager = Some(Arc::clone(&event_source_manager_arc));
                // Share workflow runtime so admin API can list/manage workflows
                if let Some(wrt) = default_service.workflow_runtime.get() {
                    state.workflow_runtime = Some(Arc::clone(wrt));
                }
                // Share playbook runtime so the admin API can list/manage playbooks
                if let Some(prt) = default_service.playbook_runtime.get() {
                    state.playbook_runtime = Some(Arc::clone(prt));
                }
            }

            // Clone lock_manager before hub_state is moved into router
            let lock_manager_for_agents = Arc::clone(&hub_state.lock_manager);
            // Cloned for the gateway nest below — both share the same hub_state.
            let hub_state_for_gateway = Arc::clone(&hub_state);

            // Built bare here; auth + CORS are layered on below, once
            // `gateway_state` exists (the auth middleware needs it as state).
            // audit F3: this admin router must NOT be served auth-free.
            let admin_app = axum::Router::new().nest("/api", workforce_hub::api::router(hub_state));

            let admin_addr: std::net::SocketAddr = wf_config
                .hub
                .listen
                .parse()
                .unwrap_or_else(|_| "0.0.0.0:8090".parse().unwrap());

            eprintln!(
                "  Admin API: {}",
                style(format!("http://{}", admin_addr)).cyan(),
            );

            // Start the WebSocket gateway (reuse broadcast_tx from above)
            let gateway_token = wf_config
                .gateway
                .auth_token
                .clone()
                .or_else(|| std::env::var("WORKFORCE_GATEWAY_TOKEN").ok())
                .unwrap_or_else(|| {
                    let token = uuid::Uuid::new_v4().to_string();
                    eprintln!(
                        "  {} Gateway token (auto-generated): {}",
                        style("🔑").bold(),
                        style(&token).yellow(),
                    );
                    token
                });

            // Heartbeat delivery routes a check's output to the broadcast
            // channel. Build it here — cloning `broadcast_tx` before the gateway
            // takes it — and hand it to the per-agent heartbeat supervisor that's
            // spawned once the agent registry exists (below). Self-learning and
            // heartbeat are per-agent: a supervisor runs one runner per agent
            // scoped to that agent's own workspace, rather than one runner bound
            // to the default agent and a hardcoded `{data_dir}/workspace`.
            // Heartbeat runners run for every agent, whether or not the
            // install turns heartbeats on: an agent's own `[heartbeat]` may
            // turn them on (or off) for it, and each runner reads it as it
            // goes. One that is off only re-reads its config each minute.
            let heartbeat_delivery: Option<workforce_hub::heartbeat::HeartbeatDeliveryCallback> = {
                let broadcast_tx_for_heartbeat = broadcast_tx.clone();
                let cb: workforce_hub::heartbeat::HeartbeatDeliveryCallback =
                    Arc::new(move |_name: String, response: String| {
                        let tx = broadcast_tx_for_heartbeat.clone();
                        Box::pin(async move {
                            let _ = tx.send(workforce_hub::gateway::BroadcastMessage::new(
                                "heartbeat",
                                "message",
                                response,
                            ));
                        })
                    });
                if wf_config.heartbeat.enabled {
                    eprintln!(
                        "  Heartbeat: {} (every {}m, active {}-{})",
                        style("enabled").cyan(),
                        wf_config.heartbeat.interval_secs / 60,
                        wf_config.heartbeat.active_start,
                        wf_config.heartbeat.active_end,
                    );
                } else {
                    eprintln!(
                        "  Heartbeat: {}",
                        style("only where an agent turns it on").dim()
                    );
                }
                Some(cb)
            };

            // Register daily briefing job if enabled (DW-168)
            if wf_config.briefing.enabled {
                let jobs = scheduler.list_jobs().await;
                let has_briefing = jobs.iter().any(|j| j.name == "daily-briefing");
                if !has_briefing {
                    match workforce_hub::daily_briefing::register_briefing_job(
                        &scheduler,
                        &wf_config.briefing,
                        wf_config.timezone.as_deref(),
                    )
                    .await
                    {
                        Ok(id) => {
                            tracing::info!(job_id = %id, "Registered daily briefing job");
                            eprintln!(
                                "  Briefing:  {} (cron: {})",
                                style("enabled").cyan(),
                                style(&wf_config.briefing.cron).cyan(),
                            );
                        }
                        Err(e) => {
                            tracing::warn!("Failed to register daily briefing: {e}");
                            eprintln!("  Briefing:  {}", style("registration failed").red(),);
                        }
                    }
                } else {
                    eprintln!(
                        "  Briefing:  {} (already registered)",
                        style("enabled").cyan(),
                    );
                }
            } else {
                eprintln!("  Briefing:  {}", style("not configured").dim());
            }

            // Build agent registry: default agent (with scheduler/reactions) + additional agents
            let default_id = wf_config
                .default_agent()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| "default".to_string());

            let mut agent_map = std::collections::HashMap::new();
            // Share the default service's local_agents and knowledge service with all agents
            let shared_local_agents = default_service.local_agents.clone();
            let shared_knowledge = default_service.knowledge_service().cloned();
            agent_map.insert(default_id.clone(), Arc::clone(&default_service));

            // Readiness is deferred to the "all surfaces registered" point
            // below, alongside the default's: a scheduler made ready here can
            // fire a persisted job before the agent registry is wired into
            // the workflow runtime, and a declared-agent step then fails with
            // "no agent registry is wired".
            let mut agent_schedulers = Vec::new();
            let mut failed_agents = Vec::new();

            for agent_config in &wf_config.agents {
                if agent_config.id == default_id {
                    continue; // Already built as the default
                }
                tracing::info!(agent = %agent_config.id, "Building additional agent");
                // Each agent owns its own scheduler (reminders, scheduled tasks
                // and the consolidation job run as that agent), not the default's.
                let built = workforce_hub::agent_registry::loaded(
                    &agent_config.id,
                    workforce_hub::serve::AgentService::build_for_agent_with_scheduler(
                        wf_config.clone(),
                        None, // config_path not available in hub server mode
                        agent_config,
                        Arc::clone(&scheduler_delivery),
                        Arc::clone(&lock_manager_for_agents),
                        shared_knowledge.clone(),
                        shared_local_agents.clone(),
                        default_service.workflow_runtime().cloned(),
                        global_vault.clone(),
                        subscriptions.clone(),
                    )
                    .await,
                );
                let (service, agent_scheduler) = match built {
                    Ok(built) => built,
                    Err(reason) => {
                        failed_agents.push((agent_config.clone(), reason));
                        continue;
                    }
                };
                if let Some(ref flag) = leader_flag {
                    agent_scheduler.set_leader_flag(Arc::clone(flag));
                }
                agent_scheduler.set_voice_announcer(Arc::clone(&voice_announcer));
                agent_schedulers.push(agent_scheduler);
                // Tick loop driven by `supervise_scheduler` (below), uniformly
                // with the default and runtime-spawned agents.
                agent_map.insert(agent_config.id.clone(), service);
            }

            let registry = Arc::new(workforce_hub::agent_registry::AgentRegistry::from_parts(
                agent_map, default_id,
            ));
            let every_agent_loaded = failed_agents.is_empty();
            for (agent_config, reason) in failed_agents {
                registry.load_failed(&agent_config, reason);
            }
            registry.wire_advisor_tools();

            workforce_hub::metabase_sync::spawn(
                Arc::clone(&registry),
                Arc::new(wf_config.clone()),
                workforce_common::metabase_schema::store(),
            );

            // Per-agent background runners. Supervisors run one self-learning
            // runner per agent (always; it no-ops while that agent has it
            // disabled) and, when heartbeat is enabled, one heartbeat runner per
            // agent — each scoped to its own `service.workspace`. They reconcile
            // against registry membership, so a runtime-spawned agent gains its
            // runners with no restart and a removed one loses them.
            let background_cancel = tokio_util::sync::CancellationToken::new();
            {
                let registry = Arc::downgrade(&registry);
                let leader_flag = leader_flag.clone();
                let user_tz = wf_config.user_timezone();
                let cancel = background_cancel.clone();
                tokio::spawn(async move {
                    workforce_hub::background::supervise_self_learning(
                        registry,
                        leader_flag,
                        user_tz,
                        cancel,
                    )
                    .await;
                });
            }
            if let Some(delivery) = heartbeat_delivery {
                let registry = Arc::downgrade(&registry);
                let leader_flag = leader_flag.clone();
                let user_tz = wf_config.user_timezone();
                let config = wf_config.heartbeat.clone();
                let cancel = background_cancel.clone();
                tokio::spawn(async move {
                    workforce_hub::background::supervise_heartbeat(
                        config,
                        registry,
                        leader_flag,
                        user_tz,
                        Some(delivery),
                        cancel,
                    )
                    .await;
                });
            }
            {
                let registry = Arc::downgrade(&registry);
                let cancel = background_cancel.clone();
                tokio::spawn(async move {
                    workforce_hub::background::supervise_scheduler(registry, cancel).await;
                });
            }

            // Inject agent registry into all workflow runtimes for per-step agent routing.
            // MCP tools are only served when the `[mcp]` section has `enabled = true`;
            // otherwise workflow engines run without them. A remote coding
            // worker is pinged once so an unreachable one is logged at boot,
            // not on the first step.
            if let Some(worker) =
                workforce_agent::tools::exec_runner::RunnerEndpoint::remote_from_config(
                    &wf_config.exec,
                )
            {
                tokio::spawn(async move {
                    workforce_hub::coding_sandbox::log_worker_health(&worker).await;
                });
            }
            for service in registry.all_services() {
                // Every agent service's background singletons (lesson decay,
                // consolidation scan, job recovery, worktree GC, training
                // cleanup) run on the leader only.
                if let Some(ref flag) = leader_flag {
                    service.set_leader_flag(Arc::clone(flag));
                }
                if let Some(wf_runtime) = service.workflow_runtime() {
                    wf_runtime.set_agent_registry(
                        Arc::clone(&registry) as Arc<dyn workforce_hub::hosts::RegistryView>
                    );
                    if let Some(patterns) = wf_config.mcp.step_patterns() {
                        wf_runtime.set_mcp_exposed_tools(patterns);
                    }
                }
            }

            // Schedule triggers arm only once the registry is wired, so a cron
            // can never fire against a runtime that cannot resolve its agent.
            // (Belt: no scheduler is `set_ready` until the surfaces block
            // below, so nothing can fire before this point either way.)
            if let Some(wf_runtime) = default_service.workflow_runtime() {
                match wf_runtime.sync_schedule_triggers().await {
                    Ok(count) => tracing::info!(count, "Armed workflow schedule triggers"),
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to arm workflow schedule triggers")
                    }
                }
            }

            // The runtime arms WorkflowStart jobs on the default agent's
            // scheduler only; one persisted on any other agent's store is a
            // stale survivor of the per-agent-runtime era and would fire
            // duplicate instances of every scheduled workflow.
            for service in registry.all_services() {
                if service.agent_id.as_str() == default_service.agent_id.as_str() {
                    continue;
                }
                if let Some(sched) = &service.scheduler {
                    let removed = sched.remove_workflow_start_jobs().await;
                    if removed > 0 {
                        tracing::info!(
                            agent = %service.agent_id,
                            removed,
                            "Swept stale workflow schedule jobs from non-owner scheduler"
                        );
                    }
                }
            }

            // Start NATS chat workers if connected
            if let Some(ref nats) = nats_for_gateway {
                workforce_hub::chat_worker::spawn_chat_workers(nats.clone(), Arc::clone(&registry));
                // Cross-instance stop: every instance listens, and every
                // service can fan a Slack "stop" out to the fleet.
                workforce_hub::stop_broadcast::spawn_stop_listener(nats.clone(), registry.clone());
                for service in registry.all_services() {
                    service.set_nats(nats.clone());
                }
                eprintln!(
                    "  NATS chat: {} ({} agent{})",
                    style("enabled").cyan(),
                    registry.len(),
                    if registry.len() == 1 { "" } else { "s" },
                );
            } else {
                eprintln!("  NATS chat: {} (standalone mode)", style("disabled").dim(),);
            }

            let agent_names: Vec<String> = registry
                .agent_ids()
                .iter()
                .map(|id| id.as_str().to_string())
                .collect();
            let registry_for_slack = Arc::clone(&registry);

            // ── Pairing services ──
            let pairing_config = wf_config.pairing.clone();
            let pairing_store: Arc<dyn workforce_pairing::PairingStore> =
                build_pairing_store(&wf_config).await?;

            // ── Project store ──
            // Built early so its DB / migrations are ready before the
            // `projects.*` tools are registered on every agent.
            let project_store: Arc<dyn workforce_projects::ProjectStore> =
                build_project_store(&wf_config).await?;

            // Register the agent-driver Claude Code tool (`coding.session`) on
            // every agent now that the project store exists. Deferred to here
            // (rather than `wire_advisor_tools` above) because the tool needs
            // the project store + base data dir, which are built after the
            // registry.
            registry.wire_coding_session_tools(
                Arc::clone(&project_store),
                workforce_hub::projects::layout::WorkRoots::from_hub(&wf_config.hub),
                wf_config.mcp.enabled,
                wf_config.coding_session.clone(),
                wf_config.exec.clone(),
                &subscriptions,
            );

            // ── MCP server install store + OAuth state store ──
            // Built eagerly so DB migrations run at hub startup and
            // any backend-misconfiguration shows up in startup logs
            // (not on the first agent tool call). Both are shared
            // across every agent's `McpServerManager` (wired below
            // after the agent registry is built) and across the
            // OAuth callback router.
            let mcp_store: Arc<dyn workforce_mcp_store::McpServerStore> =
                build_mcp_store(&wf_config).await?;
            let oauth_state: Arc<dyn workforce_oauth_client::OAuthStateStore> =
                build_oauth_state_store(&wf_config)?;

            // ── MCP servers: per-agent McpServerManager wiring ──
            // Replaces the legacy inline `spawn_all` block in
            // `AgentService::build_inner`. One manager per agent
            // shares the hub-wide store + OAuth state. After
            // construction we hydrate persisted installs (covers
            // the agent-driven / UI-driven path that landed since
            // the previous hub start), then import any new TOML
            // `[mcp.servers.*]` entries the operator added.
            let mcp_http = Arc::new(reqwest::Client::new());
            let mut mcp_managers_map: std::collections::HashMap<
                String,
                Arc<workforce_hub::mcp_servers::McpServerManager>,
            > = std::collections::HashMap::new();
            for service in registry.all_services() {
                let manager = workforce_hub::mcp_servers::McpServerManager::new(
                    service.agent_id.as_str().to_string(),
                    Arc::clone(&mcp_store),
                    Arc::clone(&service.tools),
                    service.vault.clone(),
                    service.global_vault.clone(),
                    Arc::clone(&oauth_state),
                    Some(Arc::clone(&service.cost_tracker)),
                    Arc::clone(&mcp_http),
                );
                if let Err(e) = manager.hydrate_from_store().await {
                    tracing::warn!(agent = %service.agent_id, error = %e, "MCP hydrate-from-store failed at startup");
                }
                if let Err(e) = manager.import_from_toml(&wf_config.mcp).await {
                    tracing::warn!(agent = %service.agent_id, error = %e, "MCP TOML import failed at startup");
                }
                workforce_hub::mcp_servers::register_manager_tools(
                    Arc::clone(&manager),
                    Arc::clone(&service.tools),
                    wf_config.hub.public_url.clone(),
                );
                let access = Arc::new(workforce_hub::mcp_servers::McpToolAccess::new(
                    manager.registry(),
                    Arc::clone(&service.tools),
                    service.vault.clone(),
                    service.global_vault.clone(),
                ));
                service.set_tool_access(workforce_hub::wasm_skill_access::compose_tool_access(
                    &service, access,
                ));
                service.set_mcp_manager(Arc::clone(&manager));
                // Wire the broadcast sender so generated images reach gateway/iOS
                // clients (standalone, in-process delivery).
                service.set_image_broadcast(broadcast_tx.clone());
                mcp_managers_map.insert(service.agent_id.as_str().to_string(), manager);
            }
            let mcp_managers = Arc::new(McpManagerMap(parking_lot::RwLock::new(mcp_managers_map)));
            if let Some(store) = workforce_hub::serve::shared_workflow_store(&wf_config).await {
                if let Err(e) = workforce_hub::environments::migrate_legacy_environments(
                    &store,
                    &wf_config.environments,
                    &registry.all_services(),
                    every_agent_loaded,
                )
                .await
                {
                    tracing::error!(error = %e, "moving hub-wide environments into agents failed");
                }
            }

            // ── Live agent spawn/teardown from config overlay changes ──
            //
            // Polls {data_dir}/workforce.overlay.toml; on change, reloads the
            // merged config and diffs the agent set against the live registry:
            // new agents are built + fully wired (advisor, coding.session,
            // MCP manager, NATS chat) without a restart; removed agents stop
            // resolving immediately (in-flight sessions finish on their Arc);
            // changed agents are respawned. The default agent is never torn
            // down live — changes to it still need a restart.
            let overlay_kick = Arc::new(tokio::sync::Notify::new());
            // Late-bound cells: the identity store + capability gate are built
            // below (after this watcher spawns); the boot path fills these and
            // `spawn_and_wire_agent` reads them so live-spawned agents are
            // admin-gated exactly like boot-time ones.
            let watcher_identity_store: Arc<
                parking_lot::RwLock<
                    Option<Arc<dyn workforce_identity::store::IdentityAndTeamStore>>,
                >,
            > = Arc::new(parking_lot::RwLock::new(None));
            let watcher_capability_gate: Arc<
                parking_lot::RwLock<Option<Arc<dyn workforce_agent::runner::CapabilityGate>>>,
            > = Arc::new(parking_lot::RwLock::new(None));
            let slack_slot = workforce_hub::slack_onboarding::SlackSlot::default();
            spawn_overlay_agent_watcher(OverlayWatcherCtx {
                registry: Arc::clone(&registry),
                config_path: config_path.clone(),
                data_dir: std::path::PathBuf::from(&wf_config.hub.data_dir),
                lock_manager: Arc::clone(&lock_manager_for_agents),
                shared_knowledge: shared_knowledge.clone(),
                shared_local_agents: shared_local_agents.clone(),
                global_vault: global_vault.clone(),
                subscriptions: subscriptions.clone(),
                engagement_overrides: Arc::clone(&engagement_overrides),
                project_store: Arc::clone(&project_store),
                mcp_store: Arc::clone(&mcp_store),
                oauth_state: Arc::clone(&oauth_state),
                mcp_http: Arc::clone(&mcp_http),
                mcp_managers: Arc::clone(&mcp_managers),
                broadcast_tx: broadcast_tx.clone(),
                scheduler_delivery: Arc::clone(&scheduler_delivery),
                voice_announcer: Arc::clone(&voice_announcer),
                leader_flag: leader_flag.clone(),
                nats: nats_for_gateway.clone(),
                identity_store: Arc::clone(&watcher_identity_store),
                capability_gate: Arc::clone(&watcher_capability_gate),
                slack: Arc::clone(&slack_slot),
                kick: Arc::clone(&overlay_kick),
            });

            let channel_pairing = {
                // Only create the service if any platform uses non-Open policy
                let needs_pairing = pairing_config.channel_policy
                    != workforce_pairing::PairingPolicy::Open
                    || pairing_config
                        .platform_policies
                        .values()
                        .any(|p| *p != workforce_pairing::PairingPolicy::Open);
                if needs_pairing {
                    eprintln!("  Pairing:   {}", style("channel pairing enabled").cyan(),);
                    Some(Arc::new(workforce_pairing::ChannelPairingService::new(
                        Arc::clone(&pairing_store),
                        pairing_config.clone(),
                    )))
                } else {
                    None
                }
            };

            let device_pairing = if pairing_config.device_pairing_enabled {
                eprintln!("  Pairing:   {}", style("device pairing enabled").cyan(),);
                Some(Arc::new(workforce_pairing::DevicePairingService::new(
                    Arc::clone(&pairing_store),
                    pairing_config.clone(),
                )))
            } else {
                None
            };

            // Drain token — signals graceful shutdown to all dispatch loops
            let drain_token = tokio_util::sync::CancellationToken::new();

            // Node invoke pending map — shared between GatewayState and NodeInvokeTool
            let node_invoke_pending =
                Arc::<workforce_hub::node_invoke_tool::PendingInvokes>::default();

            // Register node_invoke tool on all agents so they can invoke device capabilities
            let broadcast_tx_for_tool = broadcast_tx.clone();
            for service in registry.all_services() {
                service.tools.register(std::sync::Arc::new(
                    workforce_hub::node_invoke_tool::NodeInvokeTool::new(
                        broadcast_tx_for_tool.clone(),
                        Arc::clone(&node_invoke_pending),
                    ),
                ));
            }

            // Register device pairing management tools on all agents
            if let Some(ref dp) = device_pairing {
                for service in registry.all_services() {
                    // The identity store is filled into each service's
                    // late-bound handle once it is built, below; the tools
                    // read it when they run.
                    let pairing_ctx = workforce_hub::pairing_tool::PairingToolContext {
                        device_pairing: Arc::clone(dp),
                        identity_store: Arc::clone(&service.identity_store),
                        is_admin: workforce_hub::authz::make_admin_policy(Arc::clone(
                            &service.identity_store,
                        )),
                    };
                    for tool in workforce_hub::pairing_tool::pairing_tools(pairing_ctx) {
                        service.tools.register(tool);
                    }
                }
            }

            // Build orchestrator if teams are configured
            let orchestrator: Option<Arc<workforce_orchestrator::orchestrator::Orchestrator>> =
                None;

            // `global_vault` was opened earlier (before agent construction) so
            // that per-user credential overlays could be threaded into each
            // agent's skill loader.

            // Identity store — backend selected via [identity] backend = "...".
            // A configured backend that can't be reached fails the boot rather
            // than coming up health-green with no resolvable users.
            let identity_store: Option<Arc<dyn workforce_identity::store::IdentityAndTeamStore>> = {
                let store = build_identity_store(&wf_config).await?;
                if let Err(e) =
                    workforce_hub::jwt::bootstrap_admin(&*store.clone().as_identity_store())
                {
                    tracing::warn!(error = %e, "Bootstrap admin failed");
                }
                Some(store)
            };

            // Permission store — gated by [permissions] enabled. None
            // when the staged-rollout flag is off, in which case the
            // cascade engine is silent and the legacy YAML-only policy
            // path remains authoritative.
            let permission_evaluator: Option<
                Arc<workforce_permissions::cache::PermissionEvaluator>,
            > = match build_permission_store(&wf_config).await? {
                Some(store) => {
                    // Global System-level floor: confirm destructive actions
                    // (delete/remove/…) across every tool, incl. MCP. The
                    // cascade lets more-specific rules tighten it and per-user
                    // Always-Allow loosen it per tool.
                    let system_rules = wf_config
                        .permissions
                        .destructive_baseline
                        .then(|| workforce_permissions::cascade::SystemRule {
                            tool_pattern: workforce_policy::rules::DESTRUCTIVE_PATTERN.to_string(),
                            state: workforce_permissions::types::PermissionState::Ask,
                        })
                        .into_iter()
                        .collect();
                    Some(Arc::new(
                        workforce_permissions::cache::PermissionEvaluator::new(store)
                            .with_system_rules(system_rules),
                    ))
                }
                None => None,
            };

            // Wire the cascade writer into every agent service + hub
            // state so the runner can persist Always-* choices and the
            // standalone worker process sees the same writer. Also bind
            // the evaluator itself so phase-9 knowledge-tool guards
            // pick it up.
            if let Some(ref evaluator) = permission_evaluator {
                let writer: Arc<dyn workforce_agent::runner::PermissionWriter> =
                    workforce_hub::permissions_writer::CascadePermissionWriter::new(Arc::clone(
                        evaluator,
                    ));
                let gate: Arc<dyn workforce_agent::runner::PermissionGate> = {
                    let mut gate = workforce_hub::permissions_writer::CascadePermissionGate::new(
                        Arc::clone(evaluator),
                    );
                    if let Some(ref store) = identity_store {
                        gate = gate.with_team_store(store.clone().as_team_store());
                    }
                    Arc::new(gate)
                };
                for service in registry.all_services() {
                    service.set_permission_writer(Arc::clone(&writer));
                    service.set_permission_evaluator(Arc::clone(evaluator));
                    workforce_hub::knowledge::bind_evaluator(&service, evaluator);
                    service.set_permission_gate(Arc::clone(&gate));
                }
                hub_state_for_gateway.set_permission_writer(Arc::clone(&writer));
                hub_state_for_gateway.set_permission_gate(Arc::clone(&gate));
            }

            // Capability matrix — admin-editable, hot-reloaded, never gated on
            // `[permissions] enabled` (it is core authorization). Load the boot
            // snapshot into the process-wide matrix cell, bind the override store
            // for the admin save handler, and poll the generation so CLI
            // break-glass / out-of-band edits converge without a restart.
            {
                let store = build_capability_store(&wf_config).await?;
                match store.list_overrides() {
                    Ok(overrides) => workforce_hub::authz::set_capability_matrix(
                        workforce_hub::authz::CapabilityMatrix::from_overrides(&overrides),
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to load capability overrides at boot")
                    }
                }
                workforce_hub::authz::set_capability_store(Arc::clone(&store));
                let reload_store = Arc::clone(&store);
                tokio::spawn(async move {
                    let mut last = reload_store.current_generation().unwrap_or(0);
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        match reload_store.current_generation() {
                            Ok(generation) if generation != last => {
                                match reload_store.list_overrides() {
                                    Ok(overrides) => {
                                        workforce_hub::authz::set_capability_matrix(
                                            workforce_hub::authz::CapabilityMatrix::from_overrides(
                                                &overrides,
                                            ),
                                        );
                                        last = generation;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "capability override reload failed")
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "capability generation poll failed")
                            }
                        }
                    }
                });
            }

            // Constitutional admin gate — wired ALWAYS (not gated on `[permissions]
            // enabled`, and even with no identity store: no store ⇒ no admin
            // resolves ⇒ admin tools fail closed). Removing the inline checks must
            // never leave these tools ungated, so the gate is unconditional.
            let gate = workforce_hub::cap_gate::HubCapabilityGate::new(
                identity_store
                    .as_ref()
                    .map(|s| s.clone().as_identity_store()),
                workforce_hub::authz::capability_matrix_handle(),
            );
            let cap_gate: Arc<dyn workforce_agent::runner::CapabilityGate> =
                Arc::new(match identity_store.clone() {
                    Some(store) => gate.with_role_history(store),
                    None => gate,
                });
            for service in registry.all_services() {
                service.set_capability_gate(Arc::clone(&cap_gate));
            }
            hub_state_for_gateway.set_capability_gate(Arc::clone(&cap_gate));
            *watcher_capability_gate.write() = Some(Arc::clone(&cap_gate));

            if let Some(ref store) = identity_store {
                // Attach the store itself (used by vault inline checks + live-
                // spawned agents) wherever it exists.
                let bare = store.clone().as_identity_store();
                let teams = store.clone().as_team_store();
                let tokens = store.clone().as_remote_mcp_token_store();
                for service in registry.all_services() {
                    service.set_identity_store(Arc::clone(&bare));
                    service.set_team_store(Arc::clone(&teams));
                    service.set_remote_mcp_token_store(Arc::clone(&tokens));
                }
                *watcher_identity_store.write() = Some(Arc::clone(store));
            }

            // Approval routing (designated approvers) — gated by
            // `[approvals] enabled`. Needs the permission store (for the
            // routing rules) and the identity store (to expand roles +
            // resolve Slack ids), so it only wires when both the cascade
            // and the identity store are present. The same pending store
            // backs the router's `defer`, the Slack button handler, and
            // the expiry sweeper.
            let pending_store = build_pending_approval_store(&wf_config).await?;
            if let Some(pending_store) = pending_store.clone() {
                match (permission_evaluator.as_ref(), identity_store.as_ref()) {
                    (Some(evaluator), Some(id_store)) => {
                        let default_ttl =
                            std::time::Duration::from_secs(wf_config.approvals.default_ttl_secs);
                        let mut router = workforce_hub::approvals::HubApprovalRouter::new(
                            Arc::clone(evaluator.store()),
                            id_store.clone().as_identity_store(),
                            Arc::clone(&pending_store),
                            default_ttl,
                        );
                        router = router.with_team_store(id_store.clone().as_team_store());
                        let router: Arc<dyn workforce_agent::runner::ApprovalRouter> =
                            Arc::new(router);
                        for service in registry.all_services() {
                            service.set_approval_router(Arc::clone(&router));
                            service.set_pending_approval_store(Arc::clone(&pending_store));
                        }
                        // Expiry sweeper + boot reconciliation. One per
                        // hub; resolves the requesting agent's service per
                        // row via the registry.
                        workforce_hub::approvals::sweeper::spawn(
                            Arc::clone(&registry),
                            Arc::clone(&pending_store),
                            std::time::Duration::from_secs(
                                wf_config.approvals.sweeper_interval_secs,
                            ),
                        );
                        tracing::info!("Approval routing enabled");
                    }
                    _ => tracing::warn!(
                        "[approvals] enabled but permission store or identity store is \
                             unavailable; routed approvals are inactive"
                    ),
                }
            }

            // Trace retention. One per hub, not per agent: the writer is
            // constructed per agent, but the sweep covers the whole tree.
            // Deliberately not gated on `tracing.enabled` — cleanup of what is
            // already on disk must survive capture being switched off.
            workforce_hub::trace_writer::spawn_pruner(
                workforce_hub::trace_writer::traces_dir(&wf_config),
                wf_config.tracing.retention_days,
                leader_flag.clone(),
                drain_token.clone(),
            );

            // Telemetry, cost, resolved-approval and shadow-mode retention,
            // one per hub. The shadow tables are opened only when a store is
            // shadowed; failing to open them costs the shadow, never the hub.
            let shadow_store = match workforce_hub::shadow::shared_store(&wf_config).await {
                Ok(store) => store,
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "The shadow tables cannot be opened: shadow comparisons are not kept or pruned");
                    None
                }
            };
            // Memories' vellev replica, reconciled by the leader, when
            // memories are shadowed (DW-676).
            workforce_hub::memory_shadow::start(
                &wf_config,
                shadow_store.clone(),
                leader_flag.clone(),
                drain_token.clone(),
            )
            .await;
            // Artifacts' nightly known-item runs, on the leader, when the
            // artifact index is shadowed (DW-676).
            workforce_hub::artifact_known_items::start(
                &wf_config,
                shadow_store.clone(),
                leader_flag.clone(),
                drain_token.clone(),
            );
            workforce_hub::retention::spawn(
                workforce_hub::retention::prunes(
                    &wf_config,
                    Arc::clone(&hub_state_for_gateway.telemetry_store),
                    pending_store,
                    shadow_store,
                ),
                leader_flag.clone(),
                drain_token.clone(),
            );

            let access_gate = workforce_hub::access_gate::AccessGate::with_external(
                wf_config
                    .agents
                    .iter()
                    .map(|a| (a.id.clone(), a.access.clone()))
                    .collect(),
                wf_config.sentinel.external_users.clone(),
            );
            let scope_gate = workforce_hub::scope_gate::ScopeGate::new(
                wf_config
                    .agents
                    .iter()
                    .filter_map(|a| a.scope.clone().map(|s| (a.id.clone(), s)))
                    .collect(),
            );
            registry.install_gates(workforce_hub::agent_registry::Gates {
                access: Arc::clone(&access_gate),
                scope: Arc::clone(&scope_gate),
            });

            // audit F4 (2026-06-16): the JWT signing key must be independent of
            // the static gateway/owner token. Reusing the gateway token as the
            // HS256 secret means a leaked token can forge an admin JWT for any
            // identity. Prefer an explicit `[gateway].jwt_secret` /
            // `WORKFORCE_JWT_SECRET`; fall back to the gateway token (with a
            // loud warning) so existing deployments keep working until the
            // operator provisions a separate secret.
            // F6 (2026-05-15): JwtConfig::new still validates the secret is ≥32
            // bytes — startup fails loudly on a weak secret.
            let jwt_secret = wf_config
                .gateway
                .jwt_secret
                .clone()
                .or_else(|| std::env::var("WORKFORCE_JWT_SECRET").ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "WORKFORCE_JWT_SECRET / [gateway].jwt_secret is not set — the JWT signing \
                         key falls back to the gateway token. A leaked gateway token can then forge \
                         admin JWTs for any identity (audit F4). Set an independent >=32-byte secret \
                         to separate the keys."
                    );
                    gateway_token.clone()
                });
            let jwt_config = Some(workforce_hub::jwt::JwtConfig::new(jwt_secret, 86400)?);

            // Password logins bucket per `(account, client IP)` so one user's
            // failures never lock out everyone behind a shared proxy IP. 10
            // attempts per 5-minute window before a 5-minute cooldown leaves
            // ample room for typos while still throttling per-account brute
            // force. OIDC / token logins are unaffected.
            let rate_limiter = {
                let limiter = std::sync::Arc::new(workforce_hub::rate_limit::RateLimiter::<
                    workforce_hub::rate_limit::CredentialKey,
                >::new(
                    workforce_hub::rate_limit::RateLimitConfig {
                        max_attempts: 10,
                        window: std::time::Duration::from_secs(300),
                        lockout: std::time::Duration::from_secs(300),
                        exempt_loopback: true,
                    },
                ));
                workforce_hub::rate_limit::spawn_cleanup_task(std::sync::Arc::clone(&limiter));
                limiter
            };

            // Unauthenticated pairing endpoints (request + status polling)
            // need a looser threshold than login: legitimate clients poll
            // repeatedly during the approval window. 30/min/IP is well
            // above legit traffic and far below what brute force needs
            // alongside the 12-char pairing code (~59 bits).
            let pair_rate_limiter = {
                let limiter = std::sync::Arc::new(workforce_hub::rate_limit::RateLimiter::<
                    std::net::IpAddr,
                >::new(
                    workforce_hub::rate_limit::RateLimitConfig {
                        max_attempts: 30,
                        window: std::time::Duration::from_secs(60),
                        lockout: std::time::Duration::from_secs(300),
                        exempt_loopback: true,
                    },
                ));
                workforce_hub::rate_limit::spawn_cleanup_task(std::sync::Arc::clone(&limiter));
                limiter
            };

            // Per-identity action limiter for expensive authenticated
            // operations (chat, vault writes, session mutations, agent
            // CRUD). 120/min ≈ 2 ops/sec sustained — generous for normal
            // interactive use, throttles cost-amplification abuse.
            // Closes audit finding F4 (2026-05-15).
            let action_limiter = {
                let limiter =
                    std::sync::Arc::new(workforce_hub::rate_limit::IdentityActionLimiter::new(
                        120,
                        std::time::Duration::from_secs(60),
                    ));
                let cleanup = std::sync::Arc::clone(&limiter);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        cleanup.cleanup();
                    }
                });
                limiter
            };

            // Optional OIDC provider discovery. If configured but discovery
            // fails, log the error and carry on without OIDC — the rest of
            // the auth flows still work.
            let oidc_provider: Option<Arc<workforce_hub::oidc::OidcProvider>> = match wf_config
                .gateway
                .oidc
                .clone()
            {
                Some(mut oidc_cfg) if oidc_cfg.enabled => {
                    // Expand ${vault:...} references in the client_secret before
                    // handing the config to the OIDC provider.
                    if let Some(vault) = global_vault.as_ref() {
                        use workforce_common::settings::VaultResolver;
                        if let Some(key) = oidc_cfg
                            .client_secret
                            .strip_prefix("${vault:")
                            .and_then(|s| s.strip_suffix('}'))
                        {
                            match vault.resolve(key) {
                                Some(secret) => oidc_cfg.client_secret = secret,
                                None => {
                                    tracing::warn!(
                                        key,
                                        "OIDC client_secret references vault key that does not exist"
                                    );
                                }
                            }
                        }
                    }
                    match workforce_hub::oidc::OidcProvider::discover(oidc_cfg).await {
                        Ok(provider) => Some(provider),
                        Err(e) => {
                            tracing::error!(error = ?e, "OIDC provider discovery failed; SSO will be unavailable");
                            None
                        }
                    }
                }
                _ => None,
            };

            // Startup OOB sweep: capture identity-file changes the hub never
            // intercepted (agent self-edits, container edits) into
            // def_revisions. Spawned so a slow DB never delays serving. It
            // writes to the shared revision store, so with peers only the
            // leader sweeps (after the election settles).
            if let Some(def_store) = workforce_hub::serve::shared_workflow_store(&wf_config).await {
                let sweep_registry = Arc::clone(&registry);
                let sweep_flag = leader_flag.clone();
                tokio::spawn(async move {
                    if let Some(flag) = sweep_flag {
                        tokio::time::sleep(workforce_hub::leader_gate::BOOT_SINGLETON_DELAY).await;
                        if !flag.load(std::sync::atomic::Ordering::Relaxed) {
                            return;
                        }
                    }
                    workforce_hub::serve::sweep_identity_revisions(&def_store, &sweep_registry)
                        .await;
                });
            }

            // Hand the admin router the same registry + identity store
            // the gateway will use, so cost/scheduler-history endpoints
            // can hydrate UUIDs into display names.
            hub_state_for_gateway.set_agent_registry(Arc::clone(&registry));
            if let Some(s) = identity_store.clone() {
                let identities = s.as_identity_store();
                if let Some(warning) = workforce_hub::subscription::unknown_operator(
                    &wf_config.subscription,
                    identities.as_ref(),
                ) {
                    tracing::warn!("{warning}");
                }
                hub_state_for_gateway.set_identity_store(identities);
            }

            let skills_dir = wf_config
                .skills
                .resolved_skills_dir(&wf_config.hub.data_dir);
            let external_skills =
                workforce_hub::external_skills::ExternalSkills::shared(&skills_dir);
            let gateway_state = Arc::new(workforce_hub::gateway::GatewayState {
                registry: Arc::clone(&registry),
                nats: nats_for_gateway.clone(),
                auth_token: gateway_token,
                broadcast_tx,
                progress_forwarders: Default::default(),
                channel_pairing: channel_pairing.clone(),
                device_pairing: device_pairing.clone(),
                sentinel: wf_config.sentinel.clone(),
                access_gate: Arc::clone(&access_gate),
                drain_token: drain_token.clone(),
                node_invoke_pending,
                orchestrator,
                local_agents: shared_local_agents.clone().unwrap_or_else(|| {
                    std::sync::Arc::new(
                        workforce_hub::local_agent_registry::LocalAgentRegistry::new(),
                    )
                }),
                voice_listeners: Arc::clone(&voice_listeners),
                vault: global_vault.clone(),
                artifact_store: workforce_hub::serve::shared_artifact_store(&wf_config).await,
                def_store: workforce_hub::serve::shared_workflow_store(&wf_config).await,
                overlay_kick: Some(Arc::clone(&overlay_kick)),
                workflow_runtime: default_service.workflow_runtime().cloned(),
                playbook_runtime: default_service.playbook_runtime().cloned(),
                consolidation_runtime: default_service.consolidation_runtime().cloned(),
                monitor_engine: default_service.monitor_engine().cloned(),
                engagement_overrides: Arc::clone(&engagement_overrides),
                config_path: Some(config_path.clone()),
                identity_store: identity_store.clone().map(|s| s.as_identity_store()),
                team_store: identity_store.clone().map(|s| s.as_team_store()),
                remote_mcp_tokens: identity_store
                    .clone()
                    .map(|s| s.as_remote_mcp_token_store()),
                remote_mcp: wf_config.remote_mcp.clone(),
                permission_evaluator: permission_evaluator.clone(),
                // The live `@destructive → Ask` baseline is in effect only when
                // the cascade is wired (evaluator present ⇔ `[permissions]
                // enabled`) AND the operator left `destructive_baseline` on —
                // exactly the condition that seeds the system rule above.
                destructive_baseline_active: permission_evaluator.is_some()
                    && wf_config.permissions.destructive_baseline,
                telemetry_store: Some(Arc::clone(&hub_state_for_gateway.telemetry_store)),
                jwt_config,
                rate_limiter: Some(rate_limiter),
                pair_rate_limiter: Some(pair_rate_limiter),
                action_limiter: Some(action_limiter),
                // F12 (2026-05-15): trust X-Forwarded-For from the
                // docker-swarm overlay network. Operators deploying
                // outside swarm can override via config; the empty
                // default (test harnesses) disables XFF trust entirely.
                trusted_proxies: std::sync::Arc::new(
                    workforce_hub::trusted_proxy::TrustedProxyConfig::from_strings(&[
                        "10.0.0.0/8".to_string()
                    ])
                    .expect("static CIDR parses"),
                ),
                skills_dir,
                slack: Arc::clone(&slack_slot),
                skill_catalog: Arc::new(workforce_hub::skill_catalog::SkillCatalog::live(
                    &wf_config,
                    global_vault.clone(),
                    Arc::clone(&external_skills),
                )),
                external_skills,
                oidc_provider,
            });
            if let Some(store) = identity_store.clone() {
                workforce_hub::identity_lifecycle::install(
                    workforce_hub::identity_lifecycle::Lifecycle {
                        registry: Arc::clone(&gateway_state.registry)
                            as Arc<dyn workforce_hub::hosts::RegistryView>,
                        identities: store,
                        devices: gateway_state.device_pairing.clone(),
                        channels: gateway_state.channel_pairing.clone(),
                        defs: gateway_state.def_store.clone(),
                        artifacts: gateway_state.artifact_store.clone(),
                        external_skills: Arc::clone(&gateway_state.external_skills),
                        data_dir: std::path::PathBuf::from(&wf_config.hub.data_dir),
                        runs: workforce_hub::identity_lifecycle::Runs::until(
                            background_cancel.clone(),
                        ),
                    },
                );
            }

            // audit F3: gate the standalone admin port (binds 0.0.0.0:8090 and
            // is reachable across the flat shared VPC). Every admin endpoint now
            // requires SystemAdmin auth EXCEPT the operational scrape/probe
            // endpoints. Auth is the inner layer; CORS is outer so preflight is
            // handled before auth (matching the gateway-nested admin mount).
            let admin_app = admin_app
                .layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&gateway_state),
                    workforce_hub::gateway::require_admin_auth_except_operational,
                ))
                .layer(workforce_hub::gateway::build_cors_layer(
                    wf_config.gateway.cors,
                    &wf_config.gateway.cors_allowed_origins,
                ));

            // Merge webhook router into gateway (with workflow trigger support)
            let webhook_runtime = default_service.workflow_runtime().cloned();
            let webhook_router = workforce_hub::serve::create_webhook_router(
                Arc::clone(&default_service),
                webhook_runtime,
            );
            // HTTP trigger receiver (`POST /triggers/{path}`); only when a
            // workflow runtime is present to resolve and start workflows.
            let trigger_router = match default_service.workflow_runtime().cloned() {
                Some(rt) => {
                    workforce_hub::serve::create_trigger_router(Arc::clone(&default_service), rt)
                }
                None => axum::Router::new(),
            };
            // Mount the admin router inside the gateway at /api/admin so the
            // dashboard, health, cost, etc. endpoints are reachable from the
            // same origin as the UI in production deployments where the
            // standalone admin port is not externally exposed. Auth is
            // enforced via the same check_auth used by other gateway routes.
            let admin_nested = axum::Router::new()
                .nest(
                    "/api/admin",
                    workforce_hub::api::router(hub_state_for_gateway),
                )
                .layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&gateway_state),
                    workforce_hub::gateway::require_admin_auth,
                ));

            // Public OAuth callback for MCP server installs. Mounted
            // on the public gateway so vendors (Linear, Atlassian,
            // GitHub, …) can redirect users back to the hub after
            // consent. The route itself is unauthenticated — only
            // the random `state` token (set + consumed via the
            // shared `OAuthStateStore`) gates access. This mirrors
            // the public callback pattern Google / Slack OAuth
            // already use.
            let mcp_managers_lookup: Arc<dyn workforce_hub::mcp_servers::callback::ManagerLookup> =
                Arc::clone(&mcp_managers) as _;
            let mcp_callback_state = workforce_hub::mcp_servers::callback::CallbackState {
                managers: Arc::clone(&mcp_managers_lookup),
                state_store: Arc::clone(&oauth_state),
            };
            let mcp_callback_router = axum::Router::new()
                .route(
                    workforce_hub::mcp_servers::DEFAULT_CALLBACK_PATH,
                    axum::routing::get(workforce_hub::mcp_servers::callback::handle),
                )
                .with_state(mcp_callback_state);

            // REST API for the Settings UI's MCP Servers tab.
            // Operates on the hub's default agent. The router itself
            // applies `require_admin_auth` so unauthenticated callers
            // get 401 — these endpoints install/uninstall live tools
            // and start OAuth flows, never expose them publicly.
            let mcp_api_state = workforce_hub::mcp_servers::api::ApiState {
                managers: Arc::clone(&mcp_managers_lookup),
                default_agent_id: registry.default_id().to_string(),
                gateway: Arc::clone(&gateway_state),
                public_url: wf_config.hub.public_url.clone(),
            };
            let mcp_api_router = workforce_hub::mcp_servers::api::router(mcp_api_state);

            let remote_mcp_router = workforce_hub::remote_mcp::server::router(
                workforce_hub::remote_mcp::server::RemoteMcpState::new(Arc::clone(&gateway_state)),
            );

            let gateway_app = workforce_hub::gateway::router(gateway_state.clone())
                .merge(webhook_router)
                .merge(trigger_router)
                .merge(admin_nested)
                .merge(mcp_callback_router)
                .merge(mcp_api_router)
                .merge(remote_mcp_router);
            let gateway_app = workforce_hub::games_driver::mount(
                gateway_app,
                &gateway_state,
                &wf_config.games,
                std::path::Path::new(&wf_config.hub.data_dir),
            );
            let gateway_app = workforce_hub::desk_driver::mount(
                gateway_app,
                &gateway_state,
                &wf_config,
                background_cancel.clone(),
            )
            .await?
            .layer(workforce_hub::gateway::build_cors_layer(
                wf_config.gateway.cors,
                &wf_config.gateway.cors_allowed_origins,
            ));

            let gateway_addr: std::net::SocketAddr = wf_config
                .gateway
                .listen
                .parse()
                .unwrap_or_else(|_| "0.0.0.0:8091".parse().unwrap());

            eprintln!(
                "  Gateway:   {}",
                style(format!("http://{}", gateway_addr)).cyan(),
            );

            // Build channel coordinator from shared_channels config
            let coordinator = {
                let configs: Vec<workforce_hub::channel_coordinator::SharedChannelConfig> =
                    wf_config
                        .shared_channels
                        .iter()
                        .map(
                            |sc| workforce_hub::channel_coordinator::SharedChannelConfig {
                                channel_id: sc.channel_id.clone(),
                                agents: sc
                                    .agents
                                    .iter()
                                    .map(|a| workforce_hub::channel_coordinator::ChannelAgentId {
                                        agent_id: a.agent_id.clone(),
                                        bot_user_id: a.bot_user_id.clone(),
                                        display_name: a.display_name.clone(),
                                    })
                                    .collect(),
                                default_agent: sc.default_agent.clone(),
                                claim_timeout_ms: 500,
                            },
                        )
                        .collect();
                Arc::new(workforce_hub::channel_coordinator::ChannelCoordinator::new(
                    configs,
                ))
            };

            // Shared skill infrastructure: skills directory.
            let skills_dir = wf_config
                .skills
                .resolved_skills_dir(&wf_config.hub.data_dir);

            // Postgres pool initialisation moved into `HubState::from_config`
            // so the same pool is used by every store that opts in via its
            // own backend config. See `docs/POSTGRES.md`.

            // The skill loaders, slack manager, and claude-chat dispatcher
            // share the same `Arc<Vault>` opened earlier as `global_vault` —
            // each `Vault::open` allocates its own in-memory `HashMap` cache,
            // so opening the same file twice would silently hide writes made
            // through one instance from readers on the other (the per-user
            // OAuth token regression of 2026-04-29).

            // Start Slack skill via SlackManager (supports hot-reload)
            let slack_manager = {
                let manager = Arc::new(tokio::sync::Mutex::new(
                    workforce_hub::slack_manager::SlackManager::new(
                        Arc::clone(&registry_for_slack),
                        skills_dir.clone(),
                        global_vault.clone(),
                        channel_pairing.clone(),
                        wf_config.sentinel.clone(),
                        Some(Arc::clone(&engagement_overrides)),
                        drain_token.clone(),
                        Arc::clone(&access_gate),
                        wf_config.dm_routing.clone(),
                        Arc::clone(&coordinator),
                        identity_store
                            .clone()
                            .map(|s| s as Arc<dyn workforce_identity::store::IdentityStore>),
                    ),
                ));

                // Wire the Claude Code chat dispatcher (/cc + in-thread multi-turn + Stop).
                {
                    let agent_workspace =
                        std::path::PathBuf::from(&wf_config.hub.data_dir).join("workspace");
                    let data_dir = std::path::PathBuf::from(&wf_config.hub.data_dir);
                    let chat_mcp_enabled =
                        wf_config.claude_chat.effective_mcp_enabled(&wf_config.mcp);
                    // `/cc` runs through the same launcher and engine seam as
                    // `coding.session`, following its executor and engine
                    // unless `[claude_chat]` pins its own.
                    let chat_executor = wf_config
                        .claude_chat
                        .effective_executor(&wf_config.coding_session.executor);
                    let chat_engine = wf_config
                        .claude_chat
                        .effective_engine(&wf_config.coding_session.engine);
                    let profile = workforce_hub::coding_session_tool::DriverProfile::from(
                        &wf_config.coding_session,
                    );
                    let runner = workforce_agent::tools::exec_runner::RunnerEndpoint::from_config(
                        &wf_config.exec,
                    );
                    let launcher = workforce_hub::coding_sandbox::launcher_for(
                        &chat_executor,
                        &wf_config.exec,
                        workforce_hub::projects::layout::WorkRoots::from_hub(&wf_config.hub),
                        profile.setup_runner(runner.as_ref()),
                        workforce_hub::coding_sandbox::McpSurface {
                            exposed_tools: chat_mcp_enabled
                                .then(|| wf_config.claude_chat.effective_exposed_tools()),
                        },
                    )?;
                    let engines = workforce_hub::coding_engine::Engines::from_config(
                        &wf_config.coding_session,
                        &data_dir,
                    );
                    let engine = engines.configured(&chat_engine, None, None)?;
                    // `/cx` runs the same surface on the Codex chain.
                    let codex_engine = engines.configured(
                        &wf_config.codex_chat.effective_engine_led_by(
                            &wf_config.coding_session.engine,
                            workforce_common::settings::CodingEngineKind::Codex,
                        ),
                        None,
                        None,
                    )?;
                    let remote =
                        chat_executor.kind == workforce_common::settings::ExecutorKind::Remote;
                    let cc_service = workforce_hub::claude_code_chat::ClaudeChatService::new(
                        default_service.sessions.clone(),
                        launcher,
                        engine,
                        remote,
                        agent_workspace,
                        data_dir,
                        Arc::clone(&project_store),
                        Arc::clone(&registry_for_slack),
                        global_vault.clone(),
                    )
                    .with_rotation_policy(workforce_hub::cli_session::RotationPolicy::of(
                        &wf_config.claude_chat,
                    ))
                    .with_model_defaults(
                        wf_config.claude_chat.model.clone(),
                        wf_config.claude_chat.effort.clone(),
                    )
                    .with_codex_engine(
                        codex_engine,
                        wf_config.codex_chat.model.clone(),
                        wf_config.codex_chat.effort.clone(),
                    )
                    .with_scratch_retention(std::time::Duration::from_secs(
                        wf_config.projects.cc_session_ttl_secs,
                    ));
                    let cc_service = Arc::new(cc_service);
                    // Probe for `claude` on PATH at boot and refresh on a
                    // slow timer. /cc dispatch consults the result and posts
                    // a clear "rebuild required" card if the binary is
                    // missing (the harbor failure mode), instead of the
                    // opaque os-error-2 every spawn would produce.
                    cc_service.spawn_cli_health_probe().await;
                    // Hourly refresh of every project's base clones so the
                    // first /cc of the day doesn't pay the wallclock cost
                    // of fetching weeks of drift. Belt-and-braces with the
                    // on-/cc auto-refresh from PR1.
                    cc_service.spawn_periodic_project_refresh(std::time::Duration::from_secs(3600));
                    // Sweep idle per-session turn locks every 5 minutes so the
                    // `turn_locks` map doesn't grow unbounded with distinct
                    // session keys over the process lifetime.
                    cc_service.spawn_turn_lock_gc(std::time::Duration::from_secs(300));
                    manager.lock().await.set_claude_chat(cc_service);
                    let mcp_label = if chat_mcp_enabled {
                        style("on").green()
                    } else {
                        style("off").dim()
                    };
                    eprintln!(
                        "  Claude chat: {} (/cc in Slack, MCP {})",
                        style("enabled").cyan(),
                        mcp_label,
                    );
                }

                // Cross-task claim gate: during a zero-downtime deploy two hub
                // tasks connect to Slack at once; the shared store ensures exactly
                // one replies. Skipped (fail-open) when Postgres is disabled.
                if let Some(pg_cfg) = wf_config.postgres.as_ref().filter(|p| p.enabled) {
                    match workforce_common::slack_claim_postgres::SlackResponseClaimStore::new(
                        pg_cfg,
                    )
                    .await
                    {
                        Ok(store) => {
                            Arc::clone(&store).spawn_pruner(
                                std::time::Duration::from_secs(1800),
                                std::time::Duration::from_secs(3600),
                            );
                            manager.lock().await.set_slack_claim_store(store);
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            "Slack response-claim store unavailable; cross-task dedup disabled"
                        ),
                    }
                }

                // Initial startup: register the outbound surfaces on every
                // instance (so any instance's turn can post to Slack), then open
                // the ingress sockets only on the leader — or unconditionally
                // when there is no leader election (single instance).
                {
                    let mut mgr = manager.lock().await;
                    match mgr.register_surfaces_from_disk().await {
                        Ok(()) => {
                            let ws_count = mgr.workspace_count();
                            if ws_count == 0 {
                                eprintln!(
                                    "  Slack:     {}",
                                    style("pending (no workspace connected yet — an agent's slack.connect connects one)").dim(),
                                );
                            } else {
                                eprintln!(
                                    "  Slack:     {}",
                                    style(format!(
                                        "surfaces ready ({} workspace{})",
                                        ws_count,
                                        if ws_count == 1 { "" } else { "s" }
                                    ))
                                    .cyan(),
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Slack surface registration failed (can reload later): {}",
                                e
                            );
                            eprintln!("  Slack:     {}", style("not configured").dim());
                        }
                    }
                    let lead_now = leader_flag
                        .as_ref()
                        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(true);
                    if lead_now {
                        if let Err(e) = mgr.start_ingress().await {
                            tracing::warn!("Slack ingress start failed: {e}");
                        }
                    }
                }

                // Leader-only sockets: open/close Slack ingress as this instance
                // gains/loses leadership. Surfaces stay registered either way.
                if let Some(mut rx) = leader_rx {
                    let manager = Arc::clone(&manager);
                    tokio::spawn(async move {
                        while rx.changed().await.is_ok() {
                            let is_leader = *rx.borrow();
                            let mut mgr = manager.lock().await;
                            if is_leader {
                                if let Err(e) = mgr.start_ingress().await {
                                    tracing::warn!("Slack ingress start on promotion failed: {e}");
                                }
                            } else {
                                mgr.stop_ingress();
                            }
                        }
                    });
                }

                // Publish the Slack control (live-spawned agents and the
                // gateway reach it through the slot), then give every boot
                // agent its Slack tools — `slack.connect`, `slack.disconnect`,
                // `slack.status`, `system.reload_slack` — gated pre-dispatch by
                // the constitutional `HubCapabilityGate` (`VaultSharedWrite`).
                // The slot is filled before the registry is read, and a live
                // spawn reads it again after inserting, so an agent spawned
                // meanwhile is wired by one side or the other.
                let slack_control = workforce_hub::slack_onboarding::SlackControl::new(
                    Arc::clone(&manager),
                    registry_for_slack.clone(),
                    nats_for_gateway.clone(),
                    global_vault.as_ref().map_or_else(
                        workforce_common::secret_lock::LocalRefreshLock::shared,
                        |vault| {
                            workforce_common::settings::SecretStore::refresh_lock(vault.as_ref())
                        },
                    ),
                );
                let _ = slack_slot.set(Arc::clone(&slack_control));
                for agent_svc in registry_for_slack.all_services() {
                    slack_control.register_tools(&agent_svc);
                }
                let _slack_reload_listener = slack_control.spawn_reload_listener();

                // Register the per-agent engagement mode tools against the one
                // shared store — same helper the live-spawn path uses.
                for agent_svc in registry_for_slack.all_services() {
                    workforce_hub::engagement_tool::register_engagement_tools(
                        &agent_svc,
                        &engagement_overrides,
                        global_vault.clone(),
                    );
                }
                tracing::info!("Engagement mode tools registered on all agents");

                // Register session label tool on all agents
                {
                    for agent_svc in registry_for_slack.all_services() {
                        let label_tool: Arc<dyn workforce_agent::tools::Tool> =
                            Arc::new(workforce_hub::session_tool::SessionLabelTool::new(
                                Arc::clone(&agent_svc.sessions),
                            ));
                        agent_svc.register_tool(label_tool);
                    }
                    tracing::info!("Session label tool registered on all agents");
                }

                // Build the hub-wide knowledge maintenance runner first so
                // it can be wired into `projects.index`. Reuses the default
                // agent's `KnowledgeService`; the runner enumerates
                // standalone repos plus every project repo across every
                // agent and pulls into its own indexer-owned dir under
                // `{data_dir}/knowledge/checkouts/`, so agents never share
                // filesystem state with the parser.
                let maintenance_runner: Option<Arc<workforce_hub::knowledge::MaintenanceRunner>> =
                    if let Some(knowledge) = registry_for_slack
                        .all_services()
                        .into_iter()
                        .next()
                        .and_then(|svc| svc.knowledge_service().cloned())
                    {
                        let data_dir = std::path::PathBuf::from(&wf_config.hub.data_dir);
                        let workspace = data_dir.join("workspace");
                        let agent_vaults: std::collections::BTreeMap<
                            String,
                            Arc<workforce_hub::vault::Vault>,
                        > = registry_for_slack
                            .all_services()
                            .into_iter()
                            .filter_map(|svc| {
                                svc.vault.clone().map(|v| (svc.agent_id.to_string(), v))
                            })
                            .collect();

                        // DW-531: classify existing repositories rows tagged
                        // `unknown` so the agent-facing surface sees typed
                        // rows from first request. Non-fatal: a backfill
                        // failure leaves rows at the default `unknown`.
                        if let Err(e) =
                            workforce_hub::knowledge::backfill_lineage(knowledge.graph(), &data_dir)
                                .await
                        {
                            tracing::warn!(
                                error = %e,
                                "lineage backfill skipped (non-fatal); rows stay tagged 'unknown'"
                            );
                        }

                        // DW-534: wire the vector-backend health probe.
                        // The probe receives insert-batch errors from
                        // CodeVectorStore and combines them with the
                        // readiness check; a background task logs every
                        // state transition. Disabled cleanly when no
                        // vector store is configured.
                        let probe = Arc::new(workforce_hub::knowledge::BackendHealthProbe::new(
                            knowledge.vector_store().clone(),
                        ));
                        if let Some(vs) = knowledge.vector_store() {
                            vs.set_insert_error_sink(Arc::clone(&probe)
                                as Arc<
                                    dyn workforce_codebase_knowledge::vector::health::InsertErrorSink,
                                >);
                        }
                        spawn_vector_backend_health_logger(Arc::clone(&probe), drain_token.clone());

                        Some(Arc::new(workforce_hub::knowledge::MaintenanceRunner::new(
                            knowledge,
                            Arc::clone(&project_store),
                            data_dir,
                            workspace,
                            agent_vaults,
                            global_vault.clone(),
                            wf_config.projects.update_concurrency,
                        )))
                    } else {
                        None
                    };

                // Register projects.* tools on every agent. Each tool
                // captures the agent_id at construction so projects stay
                // scoped to the agent that owns them. `projects.index`
                // is included whenever the maintenance runner exists.
                {
                    let data_dir = std::path::PathBuf::from(&wf_config.hub.data_dir);
                    for agent_svc in registry_for_slack.all_services() {
                        let ctx = workforce_hub::projects::ProjectsToolsContext {
                            agent_id: agent_svc.agent_id.as_str().to_string(),
                            roots: workforce_hub::projects::layout::WorkRoots::from_hub(
                                &wf_config.hub,
                            ),
                            store: Arc::clone(&project_store),
                            agent_vault: agent_svc.vault.clone(),
                            global_vault: global_vault.clone(),
                            update_concurrency: wf_config.projects.update_concurrency,
                            knowledge: agent_svc.knowledge_service().cloned(),
                            runner: maintenance_runner.clone(),
                        };
                        for tool in workforce_hub::projects::project_tools(ctx) {
                            agent_svc.register_tool(tool);
                        }

                        // DW-508: knowledge.delete needs the project
                        // store + data_dir for the multi-project usage
                        // check. serve.rs's `register_knowledge_tools`
                        // already registered the unguarded variant
                        // earlier in the boot sequence; this overwrites
                        // by tool name with the guarded one for every
                        // agent that has a knowledge service.
                        if let Some(ks) = agent_svc.knowledge_service().cloned() {
                            agent_svc.register_tool(Arc::new(
                                workforce_hub::knowledge::KnowledgeDeleteTool::with_store(
                                    ks,
                                    Arc::clone(&project_store),
                                    data_dir.clone(),
                                ),
                            ));
                        }
                    }
                    tracing::info!(
                        "Projects tools ({}) registered on all agents",
                        workforce_hub::projects::TOOL_NAMES.join(", ")
                    );
                }

                // Wire `knowledge.refresh_index` + `knowledge.status`
                // + the periodic background pass on top of the runner.
                // `[knowledge].maintenance_interval_secs` sets the cadence.
                if let Some(runner) = maintenance_runner {
                    // The GitHub push webhook drives a canonical re-index pass.
                    workforce_hub::knowledge::set_maintenance_runner(
                        &default_service,
                        Arc::clone(&runner),
                    );
                    // Canonical name → repo_id resolver for the query tools, so
                    // an agent's `knowledge.ask` reaches the maintained index
                    // for the repo it names rather than the service's own root.
                    let knowledge_data_dir = std::path::PathBuf::from(&wf_config.hub.data_dir);
                    let knowledge_resolver = Arc::new(workforce_hub::knowledge::RepoResolver::new(
                        Arc::clone(&project_store),
                        knowledge_data_dir.clone(),
                        knowledge_data_dir.join("workspace"),
                    ));
                    // Know-how checks code locations against the same catalog
                    // and code graph the code tools read.
                    workforce_hub::know_how::code::set_code_index(Arc::new(
                        workforce_hub::know_how::code::HubCodeIndex::new(
                            Arc::clone(&knowledge_resolver),
                            Arc::clone(runner.knowledge().graph()),
                        ),
                    ));
                    for agent_svc in registry_for_slack.all_services() {
                        agent_svc.register_tool(Arc::new(
                            workforce_hub::knowledge::KnowledgeRefreshIndexTool::new(Arc::clone(
                                &runner,
                            )),
                        ));
                        workforce_hub::knowledge::rebind_tools(
                            &agent_svc,
                            &knowledge_resolver,
                            &runner,
                        );
                        if let Some(ks) = agent_svc.knowledge_service().cloned() {
                            let probe =
                                Arc::new(workforce_hub::knowledge::BackendHealthProbe::new(
                                    ks.vector_store().clone(),
                                ));
                            agent_svc.register_tool(Arc::new(
                                workforce_hub::knowledge::KnowledgeStatusTool::new(
                                    ks,
                                    Arc::clone(&runner),
                                    probe,
                                ),
                            ));
                        }
                    }

                    // One-time cleanup: drop orphan repository indexes (former
                    // in-place roots / agent-dir clones from older layouts) so
                    // there is one index per repo. Spawned off the startup
                    // path; destructive against shared stores, so in a
                    // clustered deployment only the leader prunes, after the
                    // election has settled.
                    let prune_runner = Arc::clone(&runner);
                    let prune_flag = leader_flag.clone();
                    tokio::spawn(async move {
                        if prune_flag.is_some() {
                            tokio::time::sleep(workforce_hub::leader_gate::BOOT_SINGLETON_DELAY)
                                .await;
                            let lead = prune_flag
                                .as_ref()
                                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                                .unwrap_or(true);
                            if !lead {
                                return;
                            }
                        }
                        match prune_runner.prune_orphans().await {
                            Ok(dropped) if !dropped.is_empty() => {
                                tracing::info!(?dropped, "pruned orphan knowledge indexes")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "orphan index prune failed"),
                        }
                    });

                    let interval = std::time::Duration::from_secs(
                        wf_config.knowledge.maintenance_interval_secs,
                    );
                    let drain = drain_token.clone();
                    let bg_runner = Arc::clone(&runner);
                    let maintenance_flag = leader_flag.clone();
                    let recheck_config = wf_config.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = drain.cancelled() => break,
                                _ = tokio::time::sleep(interval) => {
                                    // Shared vector index — one writer per fleet.
                                    let lead = maintenance_flag
                                        .as_ref()
                                        .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                                        .unwrap_or(true);
                                    if !lead {
                                        continue;
                                    }
                                    match bg_runner
                                        .run_pass(workforce_hub::knowledge::IndexMode::Incremental)
                                        .await
                                    {
                                        Ok(workforce_hub::knowledge::PassRunOutcome::Completed(report)) => {
                                            tracing::info!(
                                                target_count = report.target_count,
                                                duration_ms = report.duration_ms,
                                                "knowledge maintenance pass succeeded",
                                            );
                                            workforce_hub::know_how::stale::after_code_maintenance(
                                                &recheck_config,
                                            )
                                            .await;
                                        }
                                        Ok(workforce_hub::knowledge::PassRunOutcome::AlreadyInFlight { .. }) => {
                                            tracing::info!(
                                                "knowledge maintenance pass skipped — another pass already in flight",
                                            )
                                        }
                                        // The scheduler uses the blocking `run_pass`, which never
                                        // returns `Started` (that's `start_pass` only); arm kept
                                        // for exhaustiveness.
                                        Ok(workforce_hub::knowledge::PassRunOutcome::Started { .. }) => {}
                                        Err(e) => tracing::warn!(
                                            error = %e,
                                            "knowledge maintenance pass errored",
                                        ),
                                    }
                                }
                            }
                        }
                    });
                    tracing::info!(
                        interval_secs = wf_config.knowledge.maintenance_interval_secs,
                        "Hub-wide knowledge maintenance runner registered"
                    );
                } else {
                    tracing::info!(
                        "Hub-wide knowledge maintenance skipped — no knowledge service on any agent"
                    );
                }

                manager
            };

            // Start GitHub skill if configured
            {
                use workforce_skills::adapters::github::{GithubSkill, GithubSkillConfig};

                let github_skill_dir = skills_dir.join("github-integration");

                let github_config: Option<GithubSkillConfig> = if let Ok(Some(loaded)) =
                    workforce_skills::config_loader::load_skill_config_with_vault(
                        &github_skill_dir,
                        global_vault
                            .as_ref()
                            .map(|v| v.as_ref() as &dyn workforce_common::settings::VaultResolver),
                    ) {
                    tracing::info!(
                        path = %github_skill_dir.display(),
                        "Loaded GitHub config from skill directory"
                    );
                    match workforce_skills::config_loader::deserialize_skill_config(&loaded) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            tracing::error!("Failed to parse GitHub skill config: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };

                if let Some(config) = github_config {
                    match GithubSkill::new(config) {
                        Ok(skill) => {
                            let tools = skill.tools();
                            let tool_count = tools.len();
                            for tool in tools {
                                for agent_svc in registry_for_slack.all_services() {
                                    agent_svc.register_tool(Arc::clone(&tool));
                                }
                            }
                            eprintln!(
                                "  GitHub:    {}",
                                style(format!("connected ({} tools)", tool_count)).cyan(),
                            );
                        }
                        Err(e) => {
                            tracing::error!("Failed to initialize GitHub skill: {}", e);
                            eprintln!("  GitHub:    {}", style("init failed").red());
                        }
                    }
                } else {
                    eprintln!("  GitHub:    {}", style("not configured").dim());
                }
            }

            // All surfaces registered — allow the schedulers to fire jobs.
            // Every agent's scheduler goes ready here, after the workflow
            // runtime has its agent registry: readying one earlier reopens
            // the boot window where a persisted cron fires a declared-agent
            // step that cannot resolve.
            scheduler.set_ready();
            for agent_scheduler in &agent_schedulers {
                agent_scheduler.set_ready();
            }

            eprintln!("{}", style("─".repeat(50)).dim());
            for name in &agent_names {
                eprintln!("  {} @{}", style("✓").green().bold(), name);
            }
            eprintln!("  Waiting for messages...");

            // Liveness heartbeat: mark this process alive in the shared store
            // every 15s. Claim acquisition steals from owners whose beat has
            // gone stale (crash), freeing their sessions in under two minutes
            // instead of a full claim TTL. Runs until process exit — a
            // draining instance keeps beating so its finishing turns aren't
            // stolen mid-drain; the stale row it leaves behind IS the death
            // signal.
            if let Some(svc) = registry_for_slack.default_agent() {
                let sessions = Arc::clone(&svc.sessions);
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                    let mut consecutive_failures: u32 = 0;
                    loop {
                        tick.tick().await;
                        let s = Arc::clone(&sessions);
                        let beat =
                            tokio::task::spawn_blocking(move || s.heartbeat_instance()).await;
                        match beat {
                            Ok(Ok(())) => consecutive_failures = 0,
                            Ok(Err(e)) => {
                                consecutive_failures += 1;
                                // Sustained failure means our claims become
                                // stealable while our turns still run — loud.
                                if consecutive_failures >= 3 {
                                    tracing::error!(
                                        error = %e,
                                        consecutive_failures,
                                        "Instance heartbeat failing — live turn claims may be stolen"
                                    );
                                } else {
                                    tracing::warn!(error = %e, "Instance heartbeat failed");
                                }
                            }
                            Err(join_err) if join_err.is_cancelled() => break,
                            Err(join_err) => {
                                tracing::warn!(error = %join_err, "Instance heartbeat task panicked");
                            }
                        }
                    }
                });
            }

            // Resume any sessions that were interrupted by a previous instance
            // crash/restart. Delayed: during a start-first rolling deploy the
            // outgoing instance is still draining its turns when this instance
            // boots — resuming immediately would re-run sessions whose turns
            // are alive over there. The delay outlives the stop grace period.
            {
                let resume_registry = Arc::clone(&registry_for_slack);
                let (resume_surfaces, workspace_surfaces, resume_claude_chat): (
                    Vec<Arc<dyn workforce_common::channels::ChannelSurface>>,
                    _,
                    _,
                ) = {
                    let manager = slack_manager.lock().await;
                    (
                        manager.surfaces().to_vec(),
                        manager.workspace_surfaces(),
                        manager.claude_chat(),
                    )
                };
                workforce_hub::wire_device_logins(
                    &resume_registry,
                    workspace_surfaces,
                    session_push.clone(),
                    drain_token.clone(),
                );
                let resume_push = session_push.clone();
                let resume_delay = wf_config.hub.resume_delay_secs.unwrap_or(180);
                let dt = drain_token.clone();
                let resume_leader_flag = leader_flag.clone();
                let released =
                    workforce_hub::turn_handover::released_turns(nats_for_gateway.clone());
                tokio::spawn(async move {
                    // First pass after the boot delay, then periodically: on a
                    // standing fleet the leader at boot time may not be the
                    // instance that finds interrupted sessions later, and the
                    // sweep's own give-up branch relies on a future pass. The
                    // per-attempt marker recheck + turn claim make repeated
                    // passes idempotent.
                    let mut delay = std::time::Duration::from_secs(resume_delay);
                    loop {
                        // A draining instance that released its turns wakes
                        // the sweep at once: nothing is left running there.
                        // The instance that drained may have been the leader,
                        // so the announcement gives leadership a moment to
                        // land here before the sweep is skipped.
                        let handed_over = tokio::select! {
                            _ = tokio::time::sleep(delay) => false,
                            _ = released.notified() => true,
                            _ = dt.cancelled() => return,
                        };
                        delay = std::time::Duration::from_secs(900);
                        // With several instances running, only the leader
                        // sweeps — otherwise each would re-run the same
                        // sessions.
                        let within = match handed_over {
                            true => workforce_hub::turn_handover::LEADERSHIP_WAIT,
                            false => std::time::Duration::ZERO,
                        };
                        let is_leader = tokio::select! {
                            leading = workforce_hub::turn_handover::until_leading(
                                resume_leader_flag.as_deref(),
                                within,
                            ) => leading,
                            _ = dt.cancelled() => return,
                        };
                        if !is_leader {
                            tracing::debug!("Skipping interrupted-session sweep: not the leader");
                            continue;
                        }
                        workforce_hub::slack_manager::resume_interrupted_tasks(
                            Arc::clone(&resume_registry),
                            resume_surfaces.clone(),
                            resume_push.clone(),
                            resume_claude_chat.clone(),
                        )
                        .await;
                        workforce_hub::coding_fanout_tool::recover_waves(&resume_registry).await;
                    }
                });
            }

            // Recover workflow instances that were running before restart, and start
            // periodic maintenance to catch any that drop out at runtime.
            if let Some(wf_runtime) = default_service.workflow_runtime().cloned() {
                wf_runtime.set_shutdown_token(drain_token.clone());
                let rt = Arc::clone(&wf_runtime);
                tokio::spawn(async move {
                    rt.recover_running_instances().await;
                });

                let interval_secs = wf_config
                    .hub
                    .workflow_maintenance_interval_secs
                    .unwrap_or(300);
                let rt = Arc::clone(&wf_runtime);
                let dt = drain_token.clone();
                tokio::spawn(async move {
                    rt.start_maintenance_loop(interval_secs, dt).await;
                });
            }

            // Shutdown signal — SIGTERM (what Docker/orchestrators send) or
            // SIGINT. In order: resign leadership (successor takes over in
            // seconds instead of waiting out the lease TTL), stop background
            // supervisors, then cancel drain_token so ingress stops taking new
            // work while in-flight turns finish.
            let shutdown = {
                let dt = drain_token.clone();
                let election_cancel = election_cancel.clone();
                let bg_cancel = background_cancel.clone();
                async move {
                    #[cfg(unix)]
                    {
                        let mut term = tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::terminate(),
                        )
                        .expect("install SIGTERM handler");
                        tokio::select! {
                            _ = tokio::signal::ctrl_c() => {}
                            _ = term.recv() => {}
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        tokio::signal::ctrl_c().await.ok();
                    }
                    eprintln!("\n{} Draining in-flight tasks...", style("⏳").bold());
                    workforce_hub::shutdown::begin();
                    if let Some(cancel) = election_cancel {
                        cancel.cancel();
                    }
                    bg_cancel.cancel();
                    dt.cancel();
                }
            };

            if wf_config.mcp.enabled {
                eprintln!(
                    "  MCP: {} (each round's tools, gated as its principal)",
                    style("in process").cyan(),
                );
            }

            // Start the Microsoft Teams skill (Bot Framework). Mirrors the Slack
            // block above; Teams is push-based, so its receiver mounts on the
            // public gateway at `POST /api/messages` — a server-to-server Bot
            // Connector callback authenticated by its own JWT (no CORS) — rather
            // than holding a socket. `teams_manager` owns the dispatch loops for
            // the process lifetime.
            let mut teams_manager = workforce_hub::teams_manager::TeamsManager::new(
                registry_for_slack.clone(),
                skills_dir.clone(),
                global_vault.clone(),
                channel_pairing.clone(),
                drain_token.clone(),
            );
            let teams_router = match teams_manager.start().await {
                Ok(Some(state)) => {
                    eprintln!("  Teams:     {}", style("connected").cyan());
                    workforce_hub::teams_messages::router(Arc::new(state))
                }
                Ok(None) => {
                    eprintln!(
                        "  Teams:     {}",
                        style("pending (no credentials yet — set teams.<id>.app_id/app_password in the vault)").dim(),
                    );
                    axum::Router::new()
                }
                Err(e) => {
                    tracing::warn!("Teams startup failed (can reload later): {}", e);
                    eprintln!("  Teams:     {}", style("not configured").dim());
                    axum::Router::new()
                }
            };
            let gateway_app = gateway_app.merge(teams_router);

            // Run admin API, gateway, and Slack concurrently
            let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
            let gateway_listener = tokio::net::TcpListener::bind(gateway_addr).await?;

            // ConnectInfo<SocketAddr> is needed by the gateway's
            // trusted-proxy resolver so it can verify the *direct* peer
            // before honouring X-Forwarded-For (audit F12, 2026-05-15).
            //
            // HTTP stays up THROUGH the drain (agents finishing their turns
            // still deliver through gateway-served surfaces) and shuts down
            // gracefully afterwards — in-flight requests complete instead of
            // being aborted mid-response.
            let http_shutdown = tokio_util::sync::CancellationToken::new();
            let mut admin_task = tokio::spawn({
                let token = http_shutdown.clone();
                let serve = axum::serve(admin_listener, admin_app)
                    .with_graceful_shutdown(async move { token.cancelled().await });
                async move { serve.await }
            });
            let mut gateway_task = tokio::spawn({
                let token = http_shutdown.clone();
                let serve = axum::serve(
                    gateway_listener,
                    gateway_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move { token.cancelled().await });
                async move { serve.await }
            });
            tokio::select! {
                r = &mut admin_task => {
                    if let Ok(Err(e)) = r { eprintln!("Admin API error: {}", e); }
                }
                r = &mut gateway_task => {
                    if let Ok(Err(e)) = r { eprintln!("Gateway error: {}", e); }
                }
                _ = shutdown => {}
            }

            // Wait for in-flight turns to finish (with configurable timeout).
            // `active_runner_count` covers claimed runner slots; the in-flight
            // gauge covers every AgentRunner execution regardless of surface
            // (gateway, Teams, workflow steps, background work). Identity
            // lifecycle operations stop at their next category boundary.
            let drain_secs = wf_config.hub.drain_timeout_secs.unwrap_or(60);
            let drain_result =
                tokio::time::timeout(std::time::Duration::from_secs(drain_secs), async {
                    loop {
                        let count = registry_for_slack.active_runner_count().await
                            + workforce_agent::runner::inflight_turns()
                            + workforce_hub::identity_lifecycle::running();
                        if count == 0 {
                            break;
                        }
                        eprintln!("  {} active turn(s) — waiting...", count);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                })
                .await;

            match drain_result {
                Ok(()) => eprintln!("{} All tasks drained", style("✓").green().bold()),
                Err(_) => {
                    let remaining = registry_for_slack.active_runner_count().await
                        + workforce_agent::runner::inflight_turns()
                        + workforce_hub::identity_lifecycle::running();
                    eprintln!(
                        "{} Drain timeout ({}s) — {} turn(s) still active",
                        style("⚠").yellow().bold(),
                        drain_secs,
                        remaining,
                    );
                }
            }

            // Cancel whatever outlived the wait (their sessions stay marked
            // for the successor's resume sweep), then bulk-release every turn
            // claim this process owns — the guarantee that no conversation is
            // left locked behind a dead instance's claim.
            let (cancelled, released) = workforce_hub::shutdown::finish_drain(
                &registry_for_slack,
                std::time::Duration::from_secs(5),
            )
            .await;
            if cancelled > 0 || released > 0 {
                eprintln!(
                    "{} Shutdown: {} turn(s) cancelled for resume, {} claim(s) released",
                    style("✓").green().bold(),
                    cancelled,
                    released,
                );
                if let Some(nats) = &nats_for_gateway {
                    workforce_hub::turn_handover::announce_released(nats).await;
                }
            }

            // Drain finished — now stop accepting HTTP and let in-flight
            // requests complete, then hard-stop transport + dispatch.
            http_shutdown.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let _ = (&mut admin_task).await;
                let _ = (&mut gateway_task).await;
            })
            .await;
            slack_manager.lock().await.stop().await;
            teams_manager.stop().await;
            // Final backstop: a claim renewal already in flight when the
            // drain began can land after finish_drain's release — sweep once
            // more now that nothing can write.
            workforce_hub::shutdown::release_owned_claims(&registry_for_slack);
        }
        Commands::ExecRunner { socket, accept_uid } => {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init();

            use workforce_agent::tools::exec_runner::{serve, Listener, LocalJobHandler};
            let listener = Listener::Unix {
                path: socket,
                accept_uid,
            };
            serve(listener, Arc::new(LocalJobHandler))
                .await
                .map_err(|e| eyre::eyre!("exec runner error: {e}"))?;
        }
        Commands::PtyHolder { socket } => {
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init();
            workforce_desk::holder::run(&socket).await?;
        }
        Commands::McpServe {
            agent_id,
            exposed_tools,
            preload_skills,
            config,
            user_id,
            listen,
            auth_token_file,
        } => {
            use workforce_agent::runner::CURRENT_USER_ID;
            use workforce_common::settings::WorkforceConfig;
            use workforce_hub::mcp_serving::McpServingAgent;

            let listen_addr = match (listen, auth_token_file) {
                (Some(addr), Some(token_path)) => {
                    let token = std::fs::read_to_string(&token_path)
                        .map_err(|e| {
                            eyre::eyre!(
                                "cannot read --auth-token-file {}: {e}",
                                token_path.display()
                            )
                        })?
                        .trim()
                        .to_string();
                    if token.is_empty() {
                        return Err(eyre::eyre!(
                            "--auth-token-file {} is empty",
                            token_path.display()
                        ));
                    }
                    let listener = tokio::net::TcpListener::bind(&addr)
                        .await
                        .map_err(|e| eyre::eyre!("cannot bind --listen {addr}: {e}"))?;
                    Some((listener, token))
                }
                (None, None) => None,
                _ => {
                    return Err(eyre::eyre!(
                        "--listen and --auth-token-file must be given together"
                    ))
                }
            };

            // Route logs to stderr so stdout stays clean for JSON-RPC.
            // try_init() — yield silently if a subscriber is already installed.
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init();

            let wf_config = WorkforceConfig::load(&config)?;
            let agent_config = wf_config
                .agents
                .iter()
                .find(|a| a.id == agent_id)
                .ok_or_else(|| eyre::eyre!("Agent '{}' not found in config", agent_id))?
                .clone();

            let serving = McpServingAgent::build(wf_config, &agent_config).await?;

            // Skill preload + the JSON-RPC dispatch loop both need to see the
            // caller's identity for `LayeredUserVault` to resolve per-user
            // vault paths. Skill load discovers workspaces (e.g. Google's
            // per-user refresh tokens) so the scope must wrap preload too,
            // not just the request loop.
            let serve = async {
                let failed_skills = serving.preload_skills(&preload_skills);
                if !failed_skills.is_empty() {
                    tracing::error!(
                        agent = %agent_id,
                        skills_failed = ?failed_skills,
                        "MCP helper starting with degraded tool set — check vault credentials"
                    );
                }
                let filtered =
                    workforce_hub::mcp::build_mcp_registry(&serving.tools, &exposed_tools);

                // Honest availability report, surfaced to the model via the
                // MCP `instructions` field. The config's exposed-namespace
                // list is optimistic — without this the model sees `jira.*`
                // exposed, finds no jira tools, and invents explanations
                // ("not connected", "needs a restart") instead of saying
                // "not configured on this install".
                let served_names = filtered.names();
                let loaded: Vec<&str> = preload_skills
                    .iter()
                    .map(String::as_str)
                    .filter(|s| !failed_skills.iter().any(|(f, _)| f == s))
                    .collect();
                let dead_namespaces: Vec<String> = exposed_tools
                    .iter()
                    .filter(|pat| {
                        !served_names
                            .iter()
                            .any(|n| workforce_hub::mcp::matches_any(n, std::slice::from_ref(pat)))
                    })
                    .map(|p| p.trim_end_matches('*').trim_end_matches('.').to_string())
                    .collect();
                let mut instructions = format!(
                    "workforce MCP: {} tools served. Skills loaded: {}.",
                    served_names.len(),
                    if loaded.is_empty() {
                        "none".to_string()
                    } else {
                        loaded.join(", ")
                    },
                );
                // State facts only — never assert a cause we haven't
                // verified ("credentials expired", "not configured") or the
                // model repeats it to the user as confident misdiagnosis.
                // The reason is quoted verbatim from the loader: it is the only
                // copy that reaches anyone. This process logs to its own stderr
                // (stdout carries JSON-RPC) and nothing collects it, so
                // "check the hub logs" was pointing at a log that never had it.
                if !failed_skills.is_empty() {
                    // Flatten and cap: these strings land mid-sentence in the
                    // MCP `instructions`, and a multi-line loader error would
                    // otherwise inject newlines and kilobytes into the prompt.
                    const MAX_REASON_CHARS: usize = 200;
                    let detail = failed_skills
                        .iter()
                        .map(|(skill, reason)| {
                            let mut flat = reason.split_whitespace().collect::<Vec<_>>().join(" ");
                            if flat.chars().count() > MAX_REASON_CHARS {
                                flat =
                                    flat.chars().take(MAX_REASON_CHARS).collect::<String>() + "…";
                            }
                            format!("{skill} ({flat})")
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    instructions.push_str(&format!(
                        " Skills that failed to load, with the loader's reason — their tools are unavailable this session; report the reason verbatim if asked, do not speculate: {detail}."
                    ));
                }
                if !dead_namespaces.is_empty() {
                    instructions.push_str(&format!(
                        " No tools are served for: {} — unavailable in this session; if asked, say so plainly instead of speculating about restarts or connectivity.",
                        dead_namespaces.join(", ")
                    ));
                }
                instructions.push_str(
                    " Tool schemas may be deferred — load them via ToolSearch before calling.",
                );

                let registry = std::sync::Arc::new(filtered);
                let name = format!("workforce-{}", agent_id);
                match listen_addr {
                    Some((listener, token)) => {
                        workforce_hub::mcp::run_http_server(
                            registry,
                            name,
                            Some(instructions),
                            listener,
                            token,
                        )
                        .await
                    }
                    None => {
                        workforce_hub::mcp::run_stdio_server(registry, name, Some(instructions))
                            .await
                    }
                }
            };

            let result = match user_id {
                Some(uid) => CURRENT_USER_ID.scope(uid, serve).await,
                None => serve.await,
            };
            result.map_err(|e| eyre::eyre!("MCP stdio server error: {e}"))?;
        }
        Commands::Team { .. } | Commands::Teams { .. } => {
            eprintln!("Agent team orchestration has been removed.");
            eprintln!("Use workflows for multi-agent coordination.");
            std::process::exit(1);
        }
        Commands::Status => {
            println!("{} Workforce Status", style("📊").bold());
            println!("  NATS: not connected");
            println!("  Agents: 0 running");
            println!("  Nodes: 0 registered");
            println!("\n  Start NATS: docker compose up -d");
        }
        Commands::Init => {
            let config_dir = PathBuf::from(".");

            // Create default policy file (skip if exists)
            let policy_dir = config_dir.join("policies");
            std::fs::create_dir_all(&policy_dir)?;
            let policy_path = policy_dir.join("default.yaml");
            if !policy_path.exists() {
                std::fs::write(
                    &policy_path,
                    r#"# Workforce Policy — Default Rules
# Controls what tools agents can use. Rules are evaluated in priority order;
# first match wins. Unmatched tools are denied.
#
# Scopes: global, { agent: "id" }, { team: "id" }
# Actions: allow, deny, require_approval
# Tools: exact name or wildcard (e.g. "filesystem.*")

rules:
  # ── Filesystem ──
  # Agents can read, write, edit, and list within their workspace.
  - name: "allow-filesystem"
    scope: global
    tool: "filesystem.*"
    action: allow

  # ── Shell Execution ──
  # Agents can run shell commands.
  - name: "allow-exec"
    scope: global
    tool: "exec.run"
    action: allow

  # ── Memory ──
  # Agents can search and store memories.
  - name: "allow-memory"
    scope: global
    tool: "memory*"
    action: allow

  # ── Scheduling ──
  # Agents can schedule tasks.
  - name: "allow-scheduler"
    scope: global
    tool: "scheduler"
    action: allow

  # ── Reactions ──
  # Agents can send reactions (emoji responses, etc.).
  - name: "allow-reaction"
    scope: global
    tool: "reaction"
    action: allow

  # Any tool not matched above is denied by default.

  # ── Per-Agent Overrides (examples) ──
  # Restrict shell access to a specific agent:
  # - name: "deny-exec-for-reviewer"
  #   scope: { agent: "reviewer" }
  #   tool: "exec.run"
  #   action: deny
  #   priority: 10
"#,
                )?;
            } else {
                eprintln!(
                    "  {} policies/default.yaml already exists, skipping",
                    style("↩").dim(),
                );
            }

            // Create default config (skip if exists) or load existing
            let toml_path = config_dir.join("workforce.toml");
            let config = if toml_path.exists() {
                eprintln!(
                    "  {} workforce.toml already exists, skipping",
                    style("↩").dim(),
                );
                workforce_common::settings::WorkforceConfig::load(&toml_path).unwrap_or_default()
            } else {
                let default_toml = workforce_common::settings::WorkforceConfig::default_toml();
                std::fs::write(&toml_path, &default_toml)?;
                workforce_common::settings::WorkforceConfig::from_toml(&default_toml)
                    .unwrap_or_default()
            };

            // Determine workspace directories to initialise — one per agent.
            // Uses data/agents/{id}/ consistently, whether single or multi-agent.
            let workspaces: Vec<(String, PathBuf)> = if config.agents.is_empty() {
                // Fallback if TOML has no [[agents]] at all
                let ws = PathBuf::from(&config.hub.data_dir)
                    .join("agents")
                    .join("default");
                vec![("default".to_string(), ws)]
            } else {
                config
                    .agents
                    .iter()
                    .map(|a| {
                        let ws = a
                            .workspace
                            .as_deref()
                            .map(PathBuf::from)
                            .unwrap_or_else(|| {
                                PathBuf::from(&config.hub.data_dir)
                                    .join("agents")
                                    .join(&a.id)
                            });
                        (a.id.clone(), ws)
                    })
                    .collect()
            };

            let mut created_files: Vec<String> = Vec::new();

            // Create identity files in each workspace
            for (agent_id, workspace) in &workspaces {
                let files = write_identity_files(workspace)?;
                let prefix = workspace.display().to_string();
                for f in files {
                    created_files.push(format!("{}/{}", prefix, f));
                }
                if workspaces.len() > 1 {
                    eprintln!(
                        "  {} Initialised workspace for agent '{}'",
                        style("→").cyan(),
                        agent_id
                    );
                }
            }

            // Detect stale root-level identity files from old init
            let root_identity_files = [
                "SOUL.md",
                "TOOLS.md",
                "AGENTS.md",
                "IDENTITY.md",
                "HEARTBEAT.md",
            ];
            let stale: Vec<&str> = root_identity_files
                .iter()
                .filter(|f| config_dir.join(f).exists())
                .copied()
                .collect();
            if !stale.is_empty() {
                let ws_display = workspaces
                    .first()
                    .map(|(_, ws)| ws.display().to_string())
                    .unwrap_or_else(|| "data/workspace".to_string());
                eprintln!(
                    "\n  {} Found identity files in project root ({}).",
                    style("⚠").yellow().bold(),
                    stale.join(", ")
                );
                eprintln!(
                    "  The runtime loads identity from {}.",
                    style(&ws_display).bold()
                );
                eprintln!("  Move them:  mv {} {}/ ", stale.join(" "), ws_display);
            }

            println!("{} Initialised workforce project:", style("📁").bold());
            for f in &created_files {
                println!("  Created: {}", f);
            }
            let ws_display = workspaces
                .first()
                .map(|(_, ws)| ws.display().to_string())
                .unwrap_or_else(|| "data/workspace".to_string());
            println!(
                "\n  {} Edit {}/SOUL.md to give your agent personality. Per-user context \
                 lives in {}/users/{{identity_uuid}}.md and is auto-created on first \
                 interaction.",
                style("💡").bold(),
                ws_display,
                ws_display,
            );
            println!("  Run: workforce serve");
        }
        Commands::Node { command } => match command {
            NodeCommand::Config {
                hub_url,
                node_id,
                output,
            } => {
                let node_id = node_id.unwrap_or_else(|| {
                    hostname::get()
                        .map(|h| h.to_string_lossy().to_string())
                        .unwrap_or_else(|_| "local-node".to_string())
                });

                let config = workforce_distributed::leaf::LeafNodeConfig {
                    hub_url,
                    node_id: node_id.clone(),
                    ..Default::default()
                };

                let nats_config = config.to_nats_config();

                if let Some(path) = output {
                    std::fs::write(&path, &nats_config)?;
                    eprintln!(
                        "{} Generated leaf node config: {}",
                        style("📝").bold(),
                        style(&path).cyan()
                    );
                } else {
                    println!("{}", nats_config);
                }

                eprintln!(
                    "\n  Node ID: {}\n  Start: nats-server -c <config-file>",
                    style(&node_id).cyan()
                );
            }
            NodeCommand::Info => {
                let hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string());

                println!("{} Node Info", style("🖥️").bold());
                println!("  Hostname: {}", style(&hostname).cyan());
                println!("  Type: local");
                println!("  NATS: not connected");
            }
            NodeCommand::Start {
                hub_url,
                capabilities,
                paths,
            } => {
                let hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "local-node".to_string());

                eprintln!(
                    "{} Starting local node '{}'",
                    style("🚀").bold(),
                    style(&hostname).cyan()
                );
                eprintln!("  Hub: {}", style(&hub_url).cyan());

                if let Some(ref caps) = capabilities {
                    eprintln!("  Capabilities: {}", caps.join(", "));
                }
                if let Some(ref ps) = paths {
                    eprintln!("  Paths: {}", ps.join(", "));
                }

                eprintln!(
                    "\n  {} Node registration would connect to hub via NATS leaf node",
                    style("ℹ").blue()
                );
                eprintln!("  This requires a running NATS server. Use `workforce node config` to generate the NATS config.");
            }
            NodeCommand::Stop => {
                eprintln!("{} Stopping local node...", style("⏹").bold());
                eprintln!("  No active node connection to stop.");
            }
            NodeCommand::Status => {
                let hostname = hostname::get()
                    .map(|h| h.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "unknown".to_string());

                println!("{} Node Status", style("📊").bold());
                println!("  Hostname:      {}", style(&hostname).cyan());
                println!("  Connection:    {}", style("disconnected").yellow());
                println!("  Active agents: 0");
                println!("  Capabilities:  filesystem, docker");
            }
        },
        Commands::Skill { command } => {
            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());
            let skills_dir = workdir.join("skills");

            match command {
                SkillCommand::List => {
                    let discovered = workforce_skills::scanner::scan_skill_directories(&workdir);
                    if discovered.is_empty() {
                        println!("{} No skills found", style("📦").bold());
                        println!("\n  Scan paths:");
                        println!("    Local:  {}", skills_dir.display());
                        println!("    Global: ~/.workforce/skills/");
                        println!("\n  Install: workforce skill install <source>");
                    } else {
                        println!(
                            "{} Discovered Skills ({})",
                            style("📦").bold(),
                            discovered.len()
                        );
                        println!("{}", style("─".repeat(60)).dim());
                        for skill in &discovered {
                            let meta = &skill.skill.meta;
                            let status = if skill.is_available() {
                                style("ready").green()
                            } else {
                                style("unavailable").yellow()
                            };
                            let config_badge = match skill.config_status {
                                workforce_skills::config_loader::ConfigStatus::Configured => {
                                    style(" configured").green()
                                }
                                workforce_skills::config_loader::ConfigStatus::LegacyConfigured => {
                                    style(" legacy config").yellow()
                                }
                                workforce_skills::config_loader::ConfigStatus::Unconfigured => {
                                    style(" no config").dim()
                                }
                            };
                            println!(
                                "  {} {} (v{}) [{}|{}]",
                                style(&meta.name).cyan().bold(),
                                style(&meta.description).dim(),
                                meta.version,
                                status,
                                config_badge,
                            );
                            if !meta.tools.is_empty() {
                                println!("    Tools: {}", style(meta.tools.join(", ")).dim());
                            }
                            if !skill.is_available() {
                                let result =
                                    workforce_skills::gating::format_gating_result(&skill.gating);
                                for line in result.lines() {
                                    println!("    {}", style(line).yellow());
                                }
                            }
                        }
                        println!("{}", style("─".repeat(60)).dim());
                        println!(
                            "  {} from {}",
                            style(format!("{} skill(s)", discovered.len())).bold(),
                            style(workdir.display()).dim(),
                        );
                    }
                }
                SkillCommand::Search { query } => {
                    let discovered = workforce_skills::scanner::scan_skill_directories(&workdir);
                    let query_lower = query.to_lowercase();
                    let matches: Vec<_> = discovered
                        .iter()
                        .filter(|s| {
                            let meta = &s.skill.meta;
                            meta.name.to_lowercase().contains(&query_lower)
                                || meta.description.to_lowercase().contains(&query_lower)
                                || meta
                                    .tools
                                    .iter()
                                    .any(|t| t.to_lowercase().contains(&query_lower))
                        })
                        .collect();

                    if matches.is_empty() {
                        println!(
                            "{} No skills matching '{}'",
                            style("🔍").bold(),
                            style(&query).cyan()
                        );
                    } else {
                        println!(
                            "{} Skills matching '{}' ({}):",
                            style("🔍").bold(),
                            style(&query).cyan(),
                            matches.len()
                        );
                        for skill in &matches {
                            let meta = &skill.skill.meta;
                            let status = if skill.is_available() {
                                style("ready").green()
                            } else {
                                style("unavailable").yellow()
                            };
                            println!(
                                "  {} — {} (v{}) [{}]",
                                style(&meta.name).cyan().bold(),
                                meta.description,
                                meta.version,
                                status,
                            );
                        }
                    }
                }
                SkillCommand::Show { name } => {
                    let discovered = workforce_skills::scanner::scan_skill_directories(&workdir);
                    let found = discovered.iter().find(|s| s.skill.meta.name == name);

                    match found {
                        Some(skill) => {
                            let meta = &skill.skill.meta;
                            let status = if skill.is_available() {
                                style("ready").green()
                            } else {
                                style("unavailable").yellow()
                            };
                            println!(
                                "{} Skill: {} [{}]",
                                style("📋").bold(),
                                style(&meta.name).cyan().bold(),
                                status,
                            );
                            println!("{}", style("─".repeat(60)).dim());
                            println!("  Version:     {}", meta.version);
                            println!("  Description: {}", meta.description);
                            if !meta.author.is_empty() {
                                println!("  Author:      {}", meta.author);
                            }
                            println!("  Path:        {}", skill.path.display());

                            if !meta.tools.is_empty() {
                                println!("\n  Tools:");
                                for tool in &meta.tools {
                                    println!("    - {}", style(tool).cyan());
                                }
                            }

                            let req = &meta.requires;
                            if !req.bins.is_empty() || !req.env.is_empty() || !req.config.is_empty()
                            {
                                println!("\n  Requirements:");
                                for bin in &req.bins {
                                    println!("    bin:    {}", bin);
                                }
                                for var in &req.env {
                                    println!("    env:    {}", var);
                                }
                                for key in &req.config {
                                    println!("    config: {}", key);
                                }
                            }

                            if !skill.is_available() {
                                println!("\n  Gating:");
                                let result =
                                    workforce_skills::gating::format_gating_result(&skill.gating);
                                for line in result.lines() {
                                    println!("    {}", style(line).yellow());
                                }
                            }

                            if !skill.skill.body.is_empty() {
                                println!("\n{}", style("─".repeat(60)).dim());
                                println!("{}", skill.skill.body);
                            }
                        }
                        None => {
                            println!(
                                "{} Skill '{}' not found",
                                style("❌").bold(),
                                style(&name).cyan()
                            );
                            println!("\n  Use `workforce skill list` to see available skills.");
                        }
                    }
                }
                SkillCommand::Install { source, version: _ } => {
                    eprintln!(
                        "{} Installing skill from '{}'",
                        style("📥").bold(),
                        style(&source).cyan()
                    );

                    match workforce_skills::installer::install_skill(&source, &skills_dir) {
                        Ok(installed_path) => {
                            // Verify the installed skill
                            match workforce_skills::scanner::load_skill(&installed_path) {
                                Ok(skill) => {
                                    let meta = &skill.skill.meta;
                                    println!(
                                        "{} Installed '{}' v{} to {}",
                                        style("✅").bold(),
                                        style(&meta.name).cyan(),
                                        meta.version,
                                        style(installed_path.display()).dim(),
                                    );

                                    if !skill.is_available() {
                                        let result = workforce_skills::gating::format_gating_result(
                                            &skill.gating,
                                        );
                                        eprintln!(
                                            "\n{} Skill installed but requirements not met:",
                                            style("⚠").yellow()
                                        );
                                        for line in result.lines() {
                                            eprintln!("  {}", style(line).yellow());
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "{} Installed but failed to verify: {}",
                                        style("⚠").yellow(),
                                        e
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{} Installation failed: {}", style("❌").bold(), e);
                        }
                    }
                }
                SkillCommand::Uninstall { name } => {
                    eprintln!(
                        "{} Uninstalling skill '{}'",
                        style("🗑").bold(),
                        style(&name).cyan()
                    );

                    match workforce_skills::installer::uninstall_skill(&name, &skills_dir) {
                        Ok(()) => {
                            println!(
                                "{} Skill '{}' uninstalled",
                                style("✅").bold(),
                                style(&name).cyan()
                            );
                        }
                        Err(e) => {
                            eprintln!("{} {}", style("❌").bold(), e);
                        }
                    }
                }
                SkillCommand::Init { name, force } => {
                    // Check built-in skills first for their default templates
                    let default_toml = match name.as_str() {
                        "slack-integration" | "slack" => {
                            use workforce_skills::adapters::slack::SlackSkillConfig;
                            Some(SlackSkillConfig::default_toml())
                        }
                        _ => None,
                    };

                    let config_toml = if let Some(toml) = default_toml {
                        toml
                    } else {
                        // Try to find the skill in discovered skills for its requirements
                        let discovered =
                            workforce_skills::scanner::scan_skill_directories(&workdir);
                        let found = discovered.iter().find(|s| s.skill.meta.name == name);
                        match found {
                            Some(skill) => {
                                workforce_skills::config_loader::generate_default_config(
                                    &name,
                                    &skill.skill.meta.requires.config,
                                )
                            }
                            None => {
                                eprintln!(
                                    "{} Skill '{}' not found. Install it first or check the name.",
                                    style("❌").bold(),
                                    style(&name).cyan()
                                );
                                std::process::exit(1);
                            }
                        }
                    };

                    // Determine where to write the config
                    let effective_name = if name == "slack" {
                        "slack-integration"
                    } else {
                        &name
                    };
                    let skill_dir = {
                        let discovered =
                            workforce_skills::scanner::scan_skill_directories(&workdir);
                        match discovered
                            .iter()
                            .find(|s| s.skill.meta.name == effective_name)
                        {
                            Some(skill) => skill.path.clone(),
                            None => {
                                // For built-in skills, create directory in skills_dir
                                let dir = skills_dir.join(effective_name);
                                if let Err(e) = std::fs::create_dir_all(&dir) {
                                    eprintln!(
                                        "{} Failed to create directory {}: {}",
                                        style("❌").bold(),
                                        dir.display(),
                                        e
                                    );
                                    std::process::exit(1);
                                }
                                dir
                            }
                        }
                    };

                    let config_path = skill_dir.join("config.toml");
                    if config_path.exists() && !force {
                        eprintln!(
                            "{} Config already exists: {}",
                            style("⚠").yellow().bold(),
                            config_path.display()
                        );
                        eprintln!("  Use --force to overwrite.");
                        std::process::exit(1);
                    }

                    match std::fs::write(&config_path, &config_toml) {
                        Ok(()) => {
                            println!(
                                "{} Generated config for '{}' at {}",
                                style("✅").bold(),
                                style(effective_name).cyan(),
                                style(config_path.display()).dim()
                            );
                        }
                        Err(e) => {
                            eprintln!("{} Failed to write config: {}", style("❌").bold(), e);
                            std::process::exit(1);
                        }
                    }
                }
                SkillCommand::Catalog { source } => {
                    match workforce_common::settings::WorkforceConfig::load(
                        &workdir.join("workforce.toml"),
                    ) {
                        Ok(wf_config) => {
                            let skill_catalog = workforce_hub::skill_catalog::SkillCatalog::offline(
                                &wf_config,
                                workforce_hub::vault::open_global(&wf_config).await?,
                            );
                            match skill_catalog.list(source.as_deref()).await {
                                Ok(listing) if listing.catalogs.is_empty() => println!(
                                    "{} No matching catalogs configured. Add a [[skills.catalogs]] block to workforce.toml.",
                                    style("📚").bold()
                                ),
                                Ok(listing) => {
                                    for cat in &listing.catalogs {
                                        println!(
                                            "\n{} {} {}",
                                            style("📚").bold(),
                                            style(&cat.name).bold(),
                                            style(&cat.repo).dim()
                                        );
                                        match &cat.error {
                                            Some(err) => {
                                                println!("  {} {}", style("error:").red(), err)
                                            }
                                            None => {
                                                for s in &cat.skills {
                                                    let mark = if s.installed {
                                                        style("✓").green().to_string()
                                                    } else {
                                                        " ".to_string()
                                                    };
                                                    println!(
                                                        "  [{}] {} [{:?}] — {}",
                                                        mark,
                                                        style(&s.name).cyan(),
                                                        s.kind,
                                                        s.description
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("{} {e}", style("❌").bold());
                                    std::process::exit(1);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{} Failed to load config: {}", style("❌").bold(), e);
                            std::process::exit(1);
                        }
                    }
                }
                SkillCommand::CatalogInstall { catalog, name } => {
                    match workforce_common::settings::WorkforceConfig::load(
                        &workdir.join("workforce.toml"),
                    ) {
                        Ok(wf_config) => {
                            let skill_catalog = workforce_hub::skill_catalog::SkillCatalog::offline(
                                &wf_config,
                                workforce_hub::vault::open_global(&wf_config).await?,
                            );
                            match skill_catalog
                                .install(
                                    &workforce_hub::skill_catalog::InstallAuthority::Operator,
                                    Some(&catalog),
                                    &name,
                                )
                                .await
                            {
                                Ok(outcome) => println!(
                                    "{} {} {} from {} — {}",
                                    style("✅").bold(),
                                    if outcome.replaced {
                                        "Updated"
                                    } else {
                                        "Installed"
                                    },
                                    style(&outcome.skill).cyan(),
                                    style(&catalog).bold(),
                                    outcome.activation
                                ),
                                Err(e) => {
                                    eprintln!("{} Install failed: {}", style("❌").bold(), e);
                                    std::process::exit(1);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("{} Failed to load config: {}", style("❌").bold(), e);
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Sessions {
            history,
            redact,
            dry_run,
            agent,
            days,
            limit,
        } => {
            let admin_url = format!("http://localhost:{}", 8090);

            if redact {
                let mut url = format!("{}/api/sessions/redact?dry_run={}", admin_url, dry_run);
                if let Some(a) = &agent {
                    url.push_str(&format!("&agent={}", a));
                }
                let output = std::process::Command::new("curl")
                    .arg("-sS")
                    .arg("-X")
                    .arg("POST")
                    .arg(&url)
                    .output()
                    .expect("Failed to execute curl");
                let data: serde_json::Value = serde_json::from_slice(&output.stdout)
                    .expect("Failed to parse redaction response");
                if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                    println!("Redaction failed: {err}");
                } else {
                    let mode = if data["dry_run"].as_bool().unwrap_or(false) {
                        "would redact (dry run)"
                    } else {
                        "redacted"
                    };
                    println!(
                        "{mode}: {} of {} sessions ({} messages, {} archive messages)",
                        data["sessions_redacted"].as_u64().unwrap_or(0),
                        data["sessions_scanned"].as_u64().unwrap_or(0),
                        data["messages_redacted"].as_u64().unwrap_or(0),
                        data["archive_messages_redacted"].as_u64().unwrap_or(0),
                    );
                }
                return Ok(());
            }

            if !history {
                println!("Use --history to view session history, or --redact to scrub secrets");
                return Ok(());
            }

            // Simple HTTP request without reqwest
            let mut url = format!("{}/api/sessions/history?limit={}", admin_url, limit);
            if let Some(a) = agent {
                url.push_str(&format!("&agent={}", a));
            }
            if let Some(d) = days {
                url.push_str(&format!("&days={}", d));
            }

            // Use curl for simplicity
            let output = std::process::Command::new("curl")
                .arg("-s")
                .arg(&url)
                .output()
                .expect("Failed to execute curl");

            let data: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("Failed to parse JSON");

            if let Some(sessions) = data.get("sessions").and_then(|s| s.as_array()) {
                if sessions.is_empty() {
                    println!("No sessions found");
                } else {
                    // Simple table formatting
                    println!(
                        "{:<10} {:<10} {:<20} {:<20} {:<10} {:<10} {:<10}",
                        "Session", "Agent", "Label", "Started", "Duration", "Messages", "Tokens"
                    );
                    println!("{}", "-".repeat(90));

                    for s in sessions {
                        let id: String = s["id"].as_str().unwrap_or("").chars().take(8).collect();
                        let agent = s["agent"].as_str().unwrap_or("");
                        let label = s["label"].as_str().unwrap_or("");
                        let created = s["created_at"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(16)
                            .collect::<String>();
                        let duration = format!("{}m", s["duration_mins"].as_i64().unwrap_or(0));
                        let messages = s["messages"].as_u64().unwrap_or(0);
                        let tokens = s["tokens"].as_u64().unwrap_or(0);

                        println!(
                            "{:<10} {:<10} {:<20} {:<20} {:<10} {:<10} {:<10}",
                            id, agent, label, created, duration, messages, tokens
                        );
                    }
                }
            }
        }
        Commands::Agents { gateway, token } => {
            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());

            let wf_config =
                workforce_common::settings::WorkforceConfig::load(&workdir.join("workforce.toml"))
                    .ok();

            let gateway_url = gateway
                .or_else(|| {
                    wf_config.as_ref().map(|c| {
                        let listen = &c.gateway.listen;
                        if listen.starts_with("ws://") || listen.starts_with("wss://") {
                            listen.clone()
                        } else {
                            format!("ws://{}", listen)
                        }
                    })
                })
                .unwrap_or_else(|| "ws://localhost:8091".to_string());

            let gateway_token = token
                .or_else(|| std::env::var("WORKFORCE_GATEWAY_TOKEN").ok())
                .or_else(|| {
                    wf_config
                        .as_ref()
                        .and_then(|c| c.gateway.auth_token.clone())
                });

            let gw_token = gateway_token.ok_or_else(|| {
                eyre::eyre!("No gateway token. Use --token or set WORKFORCE_GATEWAY_TOKEN")
            })?;

            let rest = workforce_gateway_client::GatewayRestClient::new(&gateway_url, &gw_token);
            let resp = rest.agents().await?;

            println!("{} Available Agents", style("👥").bold());
            println!("{}", style("─".repeat(60)).dim());

            for agent in &resp.agents {
                let name = agent.name.as_deref().unwrap_or(&agent.id);
                let default_tag = if agent.default {
                    format!(" {}", style("(default)").green())
                } else {
                    String::new()
                };
                let model_tag = agent
                    .model
                    .as_deref()
                    .map(|m| format!(" [{}]", style(m).dim()))
                    .unwrap_or_default();

                println!(
                    "  {}{}{}",
                    style(name).cyan().bold(),
                    default_tag,
                    model_tag
                );
                println!("    id: {}", style(&agent.id).dim());

                if !agent.capabilities.is_empty() {
                    println!(
                        "    capabilities: {}",
                        style(agent.capabilities.join(", ")).dim()
                    );
                }
            }

            println!("{}", style("─".repeat(60)).dim());
            println!(
                "\n  Use: workforce chat --agent <id> --gateway {} --token <token>",
                gateway_url
            );
        }
        Commands::Tools => {
            // MemorySearchTool removed — consolidated into MemoryTool

            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());

            // Build the full tool set (same as chat/run modes)
            let tools = default_tools(vec![workdir.clone()], ToolSecrets::from_env());
            // memory tool requires a store (runtime service), listed manually below

            // Note: memory (vector) and scheduler tools require runtime services,
            // so we list them manually below.
            let definitions = tools.definitions();

            println!("{} Available Tools", style("🔧").bold());
            println!("{}", style("─".repeat(60)).dim());

            for def in &definitions {
                println!("  {} — {}", style(&def.name).cyan().bold(), def.description,);
            }

            // List runtime-only tools that aren't in the registry without a running service
            let memory_desc =
                "Store, recall, and manage memories in the vector memory store (requires Weaviate)";
            let scheduler_desc = "Create, list, and remove scheduled jobs for reminders and recurring tasks (requires serve mode)";
            println!("  {} — {}", style("memory").cyan().bold(), memory_desc,);
            println!(
                "  {} — {}",
                style("scheduler").cyan().bold(),
                scheduler_desc,
            );

            println!("{}", style("─".repeat(60)).dim());
            println!(
                "\n  {} Total: {} tools ({} built-in + 2 runtime)",
                style("📊").bold(),
                definitions.len() + 2,
                definitions.len(),
            );
            println!(
                "\n  {} Use tool names in policy rules (e.g. \"filesystem.*\" or \"exec.run\")",
                style("💡").bold(),
            );
        }
        Commands::Plugin { command } => match command {
            PluginCommand::List => {
                println!("{} Installed Plugins", style("🔌").bold());
                println!("  (no plugins installed)");
                println!("\n  Install: workforce plugin install <source>");
            }
            PluginCommand::Install { source } => {
                eprintln!(
                    "{} Installing plugin from '{}'",
                    style("📥").bold(),
                    style(&source).cyan()
                );
                eprintln!("  (plugin system not yet connected)");
            }
            PluginCommand::Uninstall { name } => {
                eprintln!(
                    "{} Uninstalling plugin '{}'",
                    style("🗑").bold(),
                    style(&name).cyan()
                );
                eprintln!("  Plugin '{}' is not installed.", name);
            }
            PluginCommand::Status { name } => {
                println!("{} Plugin: {}", style("🔌").bold(), style(&name).cyan());
                println!("  Status: not installed");
            }
        },
        Commands::Cron { command } => {
            use workforce_common::settings::WorkforceConfig;

            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());

            let config_path = workdir.join("workforce.toml");
            let wf_config = if config_path.exists() {
                WorkforceConfig::load(&config_path)?
            } else {
                WorkforceConfig::default()
            };

            // Build a minimal service for scheduler operations
            let (_service, scheduler) =
                workforce_hub::serve::AgentService::from_config_with_scheduler(wf_config, None)
                    .await?;

            // Load persisted jobs
            scheduler.load_jobs().await?;

            match command {
                CronCommand::List => {
                    let jobs = scheduler.list_jobs().await;
                    if jobs.is_empty() {
                        println!("{} No scheduled jobs", style("📋").bold());
                    } else {
                        println!("{} Scheduled Jobs ({}):", style("📋").bold(), jobs.len());
                        println!("{}", style("─".repeat(60)).dim());
                        for job in &jobs {
                            let status = if job.enabled {
                                style("enabled").green()
                            } else {
                                style("disabled").dim()
                            };
                            let next = job
                                .next_run
                                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                                .unwrap_or_else(|| "—".to_string());
                            println!(
                                "  {} {} [{}]",
                                style(&job.id[..8]).cyan(),
                                style(&job.name).bold(),
                                status,
                            );
                            if let Some(ref desc) = job.description {
                                println!("    {}", style(desc).dim());
                            }
                            println!("    Next: {} | Runs: {}", next, job.run_count,);
                        }
                    }
                }
                CronCommand::Add {
                    name,
                    message,
                    cron,
                    in_time,
                    every,
                    tz,
                    description,
                    channel,
                    model,
                } => {
                    use workforce_hub::scheduler::{JobPayload, Schedule};

                    let schedule = if let Some(ref cron_expr) = cron {
                        Schedule::Cron {
                            expr: cron_expr.clone(),
                            tz,
                        }
                    } else if let Some(ref in_str) = in_time {
                        let duration = workforce_hub::scheduler_tool_parse_relative_time(in_str)
                            .ok_or_else(|| eyre::eyre!("Invalid relative time '{}'", in_str))?;
                        Schedule::At {
                            at: chrono::Utc::now() + duration,
                        }
                    } else if let Some(secs) = every {
                        Schedule::Every {
                            every_secs: secs,
                            anchor: None,
                        }
                    } else {
                        return Err(eyre::eyre!("Specify a schedule: --cron, --in, or --every"));
                    };

                    let payload = if channel.is_some() {
                        JobPayload::AgentTurn {
                            message,
                            channel: None,
                            channel_id: channel,
                            thread_id: None,
                            model,
                            user_id: None,
                            max_rounds: None,
                            agent_owns_delivery: false,
                            tools: None,
                        }
                    } else {
                        JobPayload::SystemEvent {
                            text: message,
                            channel_id: None,
                            thread_id: None,
                            user_id: None,
                        }
                    };

                    match scheduler
                        .add_job_with_desc(&name, description.as_deref(), schedule, payload)
                        .await
                    {
                        Ok(job_id) => println!(
                            "{} Job created: {} ({})",
                            style("✅").bold(),
                            style(&name).cyan(),
                            style(&job_id[..8]).dim(),
                        ),
                        Err(e) => println!("{} {}", style("❌").bold(), e),
                    }
                }
                CronCommand::Remove { job_id } => {
                    if scheduler.remove_job(&job_id).await {
                        println!(
                            "{} Job removed: {}",
                            style("🗑").bold(),
                            style(&job_id[..8]).cyan()
                        );
                    } else {
                        // Try prefix match
                        let jobs = scheduler.list_jobs().await;
                        if let Some(job) = jobs.iter().find(|j| j.id.starts_with(&job_id)) {
                            let full_id = job.id.clone();
                            scheduler.remove_job(&full_id).await;
                            println!(
                                "{} Job removed: {} ({})",
                                style("🗑").bold(),
                                style(&job.name).cyan(),
                                &full_id[..8]
                            );
                        } else {
                            eprintln!("{} Job '{}' not found", style("❌").bold(), job_id);
                        }
                    }
                }
                CronCommand::Run { job_id } => {
                    // Try exact or prefix match
                    let jobs = scheduler.list_jobs().await;
                    let target = jobs
                        .iter()
                        .find(|j| j.id == job_id || j.id.starts_with(&job_id));

                    if let Some(job) = target {
                        let full_id = job.id.clone();
                        eprintln!(
                            "{} Running job '{}' ({})",
                            style("▶").bold(),
                            style(&job.name).cyan(),
                            &full_id[..8],
                        );
                        match scheduler.run_job_now(&full_id).await {
                            Some(record) => {
                                let status = if record.success {
                                    style("✓ success").green()
                                } else {
                                    style("✗ failed").red()
                                };
                                println!("\n  {} ({}ms)", status, record.duration_ms,);
                                if !record.output.is_empty() {
                                    println!("\n{}", record.output);
                                }
                            }
                            None => eprintln!("{} Job execution failed", style("❌").bold()),
                        }
                    } else {
                        eprintln!("{} Job '{}' not found", style("❌").bold(), job_id);
                    }
                }
                CronCommand::History { job_id } => {
                    let records = if let Some(ref id) = job_id {
                        scheduler.job_run_history(id).await
                    } else {
                        scheduler.run_history().await
                    };

                    if records.is_empty() {
                        println!("{} No run history", style("📋").bold());
                    } else {
                        println!(
                            "{} Run History ({} records):",
                            style("📋").bold(),
                            records.len()
                        );
                        println!("{}", style("─".repeat(60)).dim());
                        for record in records.iter().rev().take(20) {
                            let status = if record.success {
                                style("✓").green()
                            } else {
                                style("✗").red()
                            };
                            println!(
                                "  {} {} — {} ({}ms, {})",
                                status,
                                style(&record.job_name).bold(),
                                record.started_at.format("%Y-%m-%d %H:%M:%S"),
                                record.duration_ms,
                                if record.delivered {
                                    "delivered"
                                } else {
                                    "not delivered"
                                },
                            );
                        }
                    }
                }
            }
        }
        Commands::Vault { command } => {
            use workforce_hub::vault::migrate::{self, AuditQuery, MigrateOptions};
            use workforce_hub::vault::Vault;

            match command {
                VaultCommand::Init { data_dir } => {
                    std::fs::create_dir_all(&data_dir)?;

                    let key_path = data_dir.join("vault-key.txt");
                    let vault_path = data_dir.join("vault.enc");

                    if key_path.exists() {
                        eprintln!(
                            "{} Key file already exists: {}",
                            style("⚠").yellow().bold(),
                            key_path.display()
                        );
                    } else {
                        let master_key = Vault::generate_master_key();
                        std::fs::write(&key_path, &master_key)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                &key_path,
                                std::fs::Permissions::from_mode(0o600),
                            )?;
                        }
                        println!(
                            "{} Master key written to {} (mode 0600)",
                            style("🔑").bold(),
                            style(key_path.display()).cyan()
                        );
                    }

                    if vault_path.exists() {
                        eprintln!(
                            "{} Vault file already exists: {}",
                            style("⚠").yellow().bold(),
                            vault_path.display()
                        );
                    } else {
                        let master_key = Vault::resolve_master_key(None, &data_dir)?;
                        Vault::open(&vault_path, &master_key)?;
                        println!(
                            "{} Empty vault created at {}",
                            style("🔒").bold(),
                            style(vault_path.display()).cyan()
                        );
                    }
                }
                VaultCommand::Set {
                    path,
                    value,
                    local,
                    gateway,
                    token,
                } => {
                    let secret_value = match value {
                        Some(v) => v,
                        None => {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin().read_to_string(&mut buf)?;
                            buf.trim().to_string()
                        }
                    };

                    // Check if gateway access is requested
                    if let (Some(gateway_url), Some(auth_token)) = (gateway, token) {
                        // Remote vault access via gateway
                        let client = workforce_gateway_client::GatewayRestClient::new(
                            &gateway_url,
                            &auth_token,
                        );
                        client.vault_set(&path, &secret_value).await?;
                        println!(
                            "{} Stored: {} (remote)",
                            style("🔒").bold(),
                            style(&path).cyan()
                        );
                    } else {
                        let vault = local_vault(&local, Some("store into")).await?;
                        workforce_hub::subscription::store::set(
                            &vault,
                            &path,
                            &secret_value,
                            || vec![Arc::clone(&vault)],
                            workforce_common::settings::VaultActor::Cli,
                        )
                        .await?;
                        println!(
                            "{} Stored: {} ({})",
                            style("🔒").bold(),
                            style(&path).cyan(),
                            vault.location()
                        );
                    }
                }
                VaultCommand::Get {
                    path,
                    local,
                    gateway,
                    token,
                } => {
                    // Check if gateway access is requested
                    if let (Some(gateway_url), Some(auth_token)) = (gateway, token) {
                        // Remote vault access via gateway
                        let client = workforce_gateway_client::GatewayRestClient::new(
                            &gateway_url,
                            &auth_token,
                        );
                        match client.vault_get(&path).await {
                            Ok(value) => println!("{}", value),
                            Err(e) => {
                                eprintln!(
                                    "{} Failed to get secret '{}': {}",
                                    style("❌").bold(),
                                    style(&path).cyan(),
                                    e
                                );
                                std::process::exit(1);
                            }
                        }
                    } else {
                        let vault = local_vault(&local, None).await?;
                        match vault.get(&path) {
                            Some(value) => println!("{}", value),
                            None => {
                                eprintln!(
                                    "{} Secret '{}' not found",
                                    style("❌").bold(),
                                    style(&path).cyan()
                                );
                                std::process::exit(1);
                            }
                        }
                    }
                }
                VaultCommand::List {
                    prefix,
                    local,
                    gateway,
                    token,
                    show_source,
                } => {
                    if let (Some(gateway_url), Some(auth_token)) = (gateway, token) {
                        let client = workforce_gateway_client::GatewayRestClient::new(
                            &gateway_url,
                            &auth_token,
                        );
                        let paths = client.vault_list(prefix.as_deref()).await?;
                        if paths.is_empty() {
                            println!("{} Vault is empty (remote)", style("🔒").bold());
                        } else {
                            println!("{} Secrets ({}, remote):", style("🔒").bold(), paths.len());
                            for p in &paths {
                                println!("  {}", style(p).cyan());
                            }
                        }
                    } else {
                        let vault = local_vault(&local, None).await?;
                        let paths = vault.list(prefix.as_deref());
                        let tier = if local.agent.is_some() {
                            "agent"
                        } else {
                            "global"
                        };
                        if paths.is_empty() {
                            println!(
                                "{} Vault is empty ({})",
                                style("🔒").bold(),
                                vault.location()
                            );
                        } else {
                            println!(
                                "{} Secrets ({}, {}):",
                                style("🔒").bold(),
                                paths.len(),
                                vault.location()
                            );
                            for p in &paths {
                                if show_source {
                                    let source = if p.starts_with("users/") {
                                        format!("{tier}_user_scoped")
                                    } else {
                                        format!("{tier}_shared_bare")
                                    };
                                    println!("  {}  [{}]", style(p).cyan(), style(source).dim());
                                } else {
                                    println!("  {}", style(p).cyan());
                                }
                            }
                        }
                    }
                }
                VaultCommand::Delete {
                    path,
                    local,
                    gateway,
                    token,
                } => {
                    // Check if gateway access is requested
                    if let (Some(gateway_url), Some(auth_token)) = (gateway, token) {
                        // Remote vault access via gateway
                        let client = workforce_gateway_client::GatewayRestClient::new(
                            &gateway_url,
                            &auth_token,
                        );
                        match client.vault_delete(&path).await {
                            Ok(()) => println!(
                                "{} Deleted: {} (remote)",
                                style("🗑").bold(),
                                style(&path).cyan()
                            ),
                            Err(e) => {
                                eprintln!(
                                    "{} Failed to delete '{}': {}",
                                    style("❌").bold(),
                                    style(&path).cyan(),
                                    e
                                );
                                std::process::exit(1);
                            }
                        }
                    } else {
                        let vault = local_vault(&local, Some("delete from")).await?;
                        if workforce_hub::subscription::store::delete(
                            &vault,
                            &path,
                            workforce_common::settings::VaultActor::Cli,
                        )
                        .await?
                        {
                            println!(
                                "{} Deleted: {} ({})",
                                style("🗑").bold(),
                                style(&path).cyan(),
                                vault.location()
                            );
                        } else {
                            eprintln!(
                                "{} Secret '{}' not found",
                                style("❌").bold(),
                                style(&path).cyan()
                            );
                            std::process::exit(1);
                        }
                    }
                }
                VaultCommand::RotateKey {
                    config: Some(config),
                    new_key_file,
                    ..
                } => {
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let new_passphrase = new_vault_passphrase(new_key_file.as_deref())?;
                    let moving = new_passphrase.is_some();
                    let summary = migrate::rotate_key(&wf_config, new_passphrase).await?;
                    println!(
                        "{} Re-sealed {} secret(s) in {} vault(s) under a new key.",
                        style("🔑").bold(),
                        summary.entries,
                        summary.scopes
                    );
                    if moving {
                        println!(
                            "{} Next: give every hub the new passphrase (WORKFORCE_VAULT_KEY or its key \
                             file), redeploy, then run `workforce vault retire-key` with it configured.",
                            style("→").cyan()
                        );
                    }
                }
                VaultCommand::RotateKey { data_dir, .. } => {
                    let old_key = Vault::resolve_master_key(None, &data_dir)?;
                    let vault_path = data_dir.join("vault.enc");
                    let vault = workforce_hub::vault::FileVault::open(&vault_path, &old_key)?;

                    let new_key = Vault::generate_master_key();
                    vault.rotate_key(&new_key)?;

                    let key_path = data_dir.join("vault-key.txt");
                    std::fs::write(&key_path, &new_key)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &key_path,
                            std::fs::Permissions::from_mode(0o600),
                        )?;
                    }

                    println!(
                        "{} Key rotated. New key written to {}",
                        style("🔑").bold(),
                        style(key_path.display()).cyan()
                    );
                    println!(
                        "{} {} Remember to update WORKFORCE_VAULT_KEY if using env var",
                        style("⚠").yellow().bold(),
                        style("").bold()
                    );
                    println!(
                        "{} Restart every hub that uses this vault with the new key; until \
                         then they refuse vault writes, as they can no longer read the file.",
                        style("⚠").yellow().bold()
                    );
                }
                VaultCommand::Migrate {
                    config,
                    dry_run,
                    settle_secs,
                } => {
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let report = migrate::migrate(
                        &wf_config,
                        MigrateOptions {
                            dry_run,
                            settle: std::time::Duration::from_secs(settle_secs),
                        },
                    )
                    .await?;
                    println!(
                        "{} Vault migration to Postgres{}",
                        style("🔒").bold(),
                        if dry_run { " (dry run)" } else { "" }
                    );
                    print_vault_reports(&report.scopes);
                    for (file, scopes) in &report.shared_files {
                        println!(
                            "{} {} was shared by {}; each now has its own copy",
                            style("⚠").yellow().bold(),
                            file.display(),
                            scopes.join(" and ")
                        );
                    }
                    if report
                        .scopes
                        .iter()
                        .any(|scope| matches!(scope.outcome, migrate::Outcome::Diverged { .. }))
                    {
                        eprintln!(
                            "{} Resolve each vault marked NOT imported as \"Postgres already holds \
                             secrets\" under Troubleshooting in docs/VAULT.md describes, then run the migration \
                             again.",
                            style("❌").bold()
                        );
                        std::process::exit(1);
                    }
                    if dry_run {
                        println!("Nothing was frozen or imported.");
                    } else {
                        println!(
                            "{} Next: set [vault] backend = \"postgres\" and redeploy. Until then the \
                             vault files refuse writes; `workforce vault export` undoes the migration.",
                            style("→").cyan()
                        );
                    }
                }
                VaultCommand::Export {
                    config,
                    settle_secs,
                } => {
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let report =
                        migrate::export(&wf_config, std::time::Duration::from_secs(settle_secs))
                            .await?;
                    println!("{} Vault export to files", style("🔒").bold());
                    print_vault_reports(&report.scopes);
                    for scope in &report.unclaimed {
                        println!(
                            "{} {} stays in Postgres: no configured vault uses it",
                            style("⚠").yellow().bold(),
                            scope
                        );
                    }
                    println!(
                        "{} Next: set [vault] backend = \"file\" and redeploy. Postgres now refuses \
                         vault writes.",
                        style("→").cyan()
                    );
                }
                VaultCommand::Verify { config } => {
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let on_postgres = workforce_hub::vault::uses_postgres(&wf_config)?;
                    let checks = migrate::verify(&wf_config).await?;
                    for check in &checks {
                        let file = match (check.file_exists, check.file_frozen) {
                            (false, _) => "no file",
                            (true, true) => "file frozen",
                            (true, false) => "file writable",
                        };
                        let postgres = check.postgres.as_ref().map_or_else(
                            || "not in Postgres".to_string(),
                            |state| {
                                format!(
                                    "Postgres: {} secret(s), generation {}{}",
                                    state.entries,
                                    state.generation,
                                    if state.frozen_at.is_some() {
                                        ", frozen"
                                    } else {
                                        ""
                                    }
                                )
                            },
                        );
                        println!("  {:<24} {file} · {postgres}", style(&check.scope).cyan());
                        if check.changed_since_import {
                            println!(
                                "    {} changed after it was imported — a write got past the freeze",
                                style("✗").red()
                            );
                        }
                        if check.unmigrated {
                            println!(
                                "    {} the file was never imported — run `workforce vault migrate`",
                                style("✗").red()
                            );
                        }
                        if let Some(differences) = &check.differences {
                            [
                                ("only in the file", &differences.only_in_file),
                                ("only in Postgres", &differences.only_in_postgres),
                                ("different", &differences.changed),
                            ]
                            .into_iter()
                            .filter(|(_, paths)| !paths.is_empty())
                            .for_each(|(label, paths)| {
                                println!("    {label}: {}", paths.join(", "))
                            });
                        }
                    }
                    if checks.iter().any(|check| {
                        check.changed_since_import || (on_postgres && check.unmigrated)
                    }) {
                        std::process::exit(1);
                    }
                }
                VaultCommand::Audit {
                    config,
                    agent,
                    since,
                    limit,
                } => {
                    use workforce_hub::vault_audit::AuditAction;
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let records = migrate::audit(
                        &wf_config,
                        AuditQuery {
                            agent: agent.as_deref(),
                            since,
                            limit,
                        },
                    )
                    .await?;
                    if records.is_empty() {
                        println!("No vault audit records.");
                    }
                    for (scope, record) in &records {
                        let action = match record.action {
                            AuditAction::Set => "set",
                            AuditAction::Delete => "delete",
                        };
                        println!(
                            "{}  {:<16} {:<6} {:<28} {}{}",
                            record.ts.format("%Y-%m-%d %H:%M:%S"),
                            scope,
                            action,
                            record.actor,
                            record.path,
                            if record.ok { "" } else { "  (failed)" }
                        );
                    }
                }
                VaultCommand::RetireKey { config } => {
                    let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
                    let retired = migrate::retire_key(&wf_config).await?;
                    println!(
                        "{} Retired {retired} key wrap(s); only the configured passphrase opens the \
                         vault now.",
                        style("🔑").bold()
                    );
                }
                VaultCommand::Copy {
                    path,
                    agent,
                    from_tier,
                    to_tier,
                    from_user,
                    to_users,
                    overwrite,
                    gateway,
                    token,
                } => {
                    let client = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);
                    let resp = client
                        .agent_vault_copy(
                            &agent,
                            &path,
                            &from_tier,
                            &to_tier,
                            from_user.as_deref(),
                            &to_users,
                            overwrite,
                        )
                        .await?;
                    println!(
                        "{} Copied {} to {} user(s) on agent {}:",
                        style("🔒").bold(),
                        style(&path).cyan(),
                        resp.copied,
                        style(&agent).bold()
                    );
                    for outcome in &resp.results {
                        let mark = match outcome.status.as_str() {
                            "copied" => style("✓").green(),
                            "exists" => style("•").yellow(),
                            "source" => style("•").dim(),
                            _ => style("✗").red(),
                        };
                        let note = outcome
                            .error
                            .as_deref()
                            .map(|e| format!(" — {e}"))
                            .unwrap_or_default();
                        println!(
                            "  {} {} [{}]{}",
                            mark,
                            style(&outcome.user_id).cyan(),
                            outcome.status,
                            note
                        );
                    }
                }
            }
        }
        Commands::Pair { command, data_dir } => {
            // Handle `pair request` early — it's unauthenticated and always remote
            if let PairCommand::Request { name, gateway } = command {
                let device_name = name.unwrap_or_else(|| {
                    hostname::get()
                        .map(|h| h.to_string_lossy().to_string())
                        .unwrap_or_else(|_| "cli-device".to_string())
                });

                // Stable client ID: persisted in ~/.workforce/device_id
                let device_id_path = std::env::var("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                    .join(".workforce")
                    .join("device_id");
                let client_id = if device_id_path.exists() {
                    std::fs::read_to_string(&device_id_path)?.trim().to_string()
                } else {
                    let id = uuid::Uuid::new_v4().to_string();
                    if let Some(parent) = device_id_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&device_id_path, &id)?;
                    id
                };

                // Convert ws:// to http:// for REST calls
                let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, "");

                eprintln!(
                    "{} Requesting pairing code for \"{}\"...",
                    style("⏳").bold(),
                    style(&device_name).cyan(),
                );

                let resp = rest
                    .pair_request_device(&device_name, Some(&client_id))
                    .await?;
                let code = resp["code"]
                    .as_str()
                    .ok_or_else(|| eyre::eyre!("No code in response"))?;
                let expires_at = resp["expires_at"].as_str().unwrap_or("unknown");

                println!();
                println!("  Pairing code:  {}", style(code).cyan().bold(),);
                println!("  Expires:       {}", style(expires_at).dim(),);
                println!();
                println!(
                    "  Ask your admin to run: {} {} {}",
                    style("workforce pair approve").bold(),
                    style(code).cyan().bold(),
                    style("--gateway <URL> --token <ADMIN_TOKEN>").dim(),
                );
                println!();
                eprintln!("{} Waiting for approval...", style("⏳").bold());

                // Poll for approval
                let code = code.to_string();
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                    match rest.pair_check_status(&code).await {
                        Ok(status) => {
                            let s = status["status"].as_str().unwrap_or("unknown");
                            match s {
                                "approved" => {
                                    if let Some(token) = status["token"].as_str() {
                                        println!();
                                        println!(
                                            "{} Device paired successfully!",
                                            style("✓").green().bold(),
                                        );
                                        println!();
                                        println!("  Token: {}", style(token).yellow().bold());
                                        println!();
                                        println!(
                                            "  {}",
                                            style("Save this token — it won't be shown again.")
                                                .dim()
                                        );
                                        println!();
                                        println!("  To connect as a local agent:");
                                        println!(
                                            "    {} WORKFORCE_GATEWAY_URL {}",
                                            style("export").dim(),
                                            style(&gateway).dim(),
                                        );
                                        println!(
                                            "    {} WORKFORCE_GATEWAY_TOKEN {}",
                                            style("export").dim(),
                                            style(token).dim(),
                                        );
                                        println!(
                                            "    {}",
                                            style("workforce local-agent connect").bold(),
                                        );
                                    } else {
                                        eprintln!(
                                            "{} Pairing approved but token already claimed. Try again.",
                                            style("✗").red(),
                                        );
                                        std::process::exit(1);
                                    }
                                    break;
                                }
                                "rejected" => {
                                    eprintln!("{} Pairing request was rejected.", style("✗").red(),);
                                    std::process::exit(1);
                                }
                                "expired" => {
                                    eprintln!(
                                        "{} Pairing code expired. Please try again.",
                                        style("✗").red(),
                                    );
                                    std::process::exit(1);
                                }
                                "pending" => continue,
                                _ => continue,
                            }
                        }
                        Err(_) => continue, // network error, keep polling
                    }
                }

                return Ok(());
            }

            // Extract gateway/token from subcommand if present
            let (gateway, token) = match &command {
                PairCommand::Pending { gateway, token, .. }
                | PairCommand::Approve { gateway, token, .. }
                | PairCommand::Reject { gateway, token, .. }
                | PairCommand::Allowlist { gateway, token, .. }
                | PairCommand::Devices { gateway, token }
                | PairCommand::RevokeDevice { gateway, token, .. } => {
                    (gateway.clone(), token.clone())
                }
                _ => (None, None),
            };

            // Resolve gateway URL and token for remote mode
            let workdir = cli
                .workdir
                .canonicalize()
                .unwrap_or_else(|_| cli.workdir.clone());
            let wf_config =
                workforce_common::settings::WorkforceConfig::load(&workdir.join("workforce.toml"))
                    .ok();

            let gateway_url = gateway.or_else(|| {
                wf_config.as_ref().map(|c| {
                    let listen = &c.gateway.listen;
                    if listen.starts_with("ws://") || listen.starts_with("wss://") {
                        listen.clone()
                    } else {
                        format!("ws://{}", listen)
                    }
                })
            });

            let gateway_token = token.or_else(|| {
                wf_config
                    .as_ref()
                    .and_then(|c| c.gateway.auth_token.clone())
            });

            // If we have a gateway URL and token, try remote mode
            let remote =
                if let (Some(ref gw_url), Some(ref gw_token)) = (&gateway_url, &gateway_token) {
                    if workforce_gateway_client::gateway_available(gw_url).await {
                        Some(workforce_gateway_client::GatewayRestClient::new(
                            gw_url, gw_token,
                        ))
                    } else {
                        eprintln!(
                            "{} Gateway not available at {}, falling back to local mode",
                            style("⚠").yellow(),
                            style(gw_url).dim(),
                        );
                        None
                    }
                } else {
                    None
                };

            if let Some(rest) = remote {
                // ── Remote mode: use gateway REST API ──
                match command {
                    PairCommand::Pending { kind, .. } => {
                        let resp = rest.pair_pending(kind.as_deref()).await?;
                        let mut total = 0;

                        if let Some(channel) = resp.get("channel").and_then(|v| v.as_array()) {
                            if !channel.is_empty() && kind.as_deref().is_none_or(|k| k == "channel")
                            {
                                println!("Channel pairing requests:");
                                for req in channel {
                                    println!(
                                        "  {} [{}] {} ({})",
                                        style(req["code"].as_str().unwrap_or("?")).cyan().bold(),
                                        req["platform"].as_str().unwrap_or("?"),
                                        req["sender_name"].as_str().unwrap_or("?"),
                                        req["sender_id"].as_str().unwrap_or("?"),
                                    );
                                    total += 1;
                                }
                            }
                        }

                        if let Some(device) = resp.get("device").and_then(|v| v.as_array()) {
                            if !device.is_empty() && kind.as_deref().is_none_or(|k| k == "device") {
                                println!("Device pairing requests:");
                                for req in device {
                                    println!(
                                        "  {} {}",
                                        style(req["code"].as_str().unwrap_or("?")).cyan().bold(),
                                        req["device_name"].as_str().unwrap_or("?"),
                                    );
                                    total += 1;
                                }
                            }
                        }

                        if total == 0 {
                            println!("{} No pending pairing requests", style("✓").green());
                        }
                    }
                    PairCommand::Approve { code, .. } => {
                        let resp = rest.pair_approve(&code).await?;
                        let kind = resp["type"].as_str().unwrap_or("unknown");
                        match kind {
                            "channel" => {
                                println!(
                                    "{} Approved channel pairing for {} ({}) on {}",
                                    style("✓").green(),
                                    resp["sender_name"].as_str().unwrap_or("?"),
                                    resp["sender_id"].as_str().unwrap_or("?"),
                                    resp["code"].as_str().unwrap_or("?"),
                                );
                            }
                            "device" => {
                                println!(
                                    "{} Approved device pairing for {}",
                                    style("✓").green(),
                                    resp["device_name"].as_str().unwrap_or("?"),
                                );
                                if let Some(id) = resp["device_id"].as_str() {
                                    println!("  Device ID: {}", id);
                                }
                                if let Some(t) = resp["token"].as_str() {
                                    println!("  Token:     {}", style(t).yellow());
                                    println!(
                                        "  {}",
                                        style("Save this token — it won't be shown again.").dim()
                                    );
                                }
                            }
                            _ => {
                                println!("{} Approved pairing code {}", style("✓").green(), code,);
                            }
                        }
                    }
                    PairCommand::Reject { code, .. } => {
                        rest.pair_reject(&code).await?;
                        println!("{} Rejected pairing code {}", style("✓").green(), code);
                    }
                    PairCommand::Devices { .. } => {
                        let resp = rest.pair_devices().await?;
                        let devices = resp["devices"].as_array();
                        if devices.is_none_or(|d| d.is_empty()) {
                            println!("{} No paired devices", style("ℹ").blue());
                        } else {
                            let devices = devices.unwrap();
                            println!("Paired devices ({}):", devices.len());
                            for device in devices {
                                let revoked = device["revoked"].as_bool().unwrap_or(false);
                                let status = if revoked {
                                    style("revoked").red()
                                } else {
                                    style("active").green()
                                };
                                let id = device["id"].as_str().unwrap_or("?");
                                let short_id = if id.len() >= 8 { &id[..8] } else { id };
                                println!(
                                    "  {} {} [{}]",
                                    style(short_id).dim(),
                                    device["device_name"].as_str().unwrap_or("?"),
                                    status,
                                );
                            }
                        }
                    }
                    PairCommand::RevokeDevice { device_id, .. } => {
                        // Resolve prefix to full ID (user may paste the short 8-char ID from `pair devices`)
                        let full_id = if device_id.len() < 36 {
                            let resp = rest.pair_devices().await?;
                            let devices = resp["devices"].as_array();
                            let matches: Vec<&str> = devices
                                .into_iter()
                                .flatten()
                                .filter_map(|d| d["id"].as_str())
                                .filter(|id| id.starts_with(&device_id))
                                .collect();
                            match matches.len() {
                                0 => {
                                    eprintln!(
                                        "{} No device matching '{}'",
                                        style("✗").red(),
                                        &device_id
                                    );
                                    std::process::exit(1);
                                }
                                1 => matches[0].to_string(),
                                _ => {
                                    eprintln!("{} Ambiguous prefix '{}' — matches {} devices. Use a longer prefix.", style("✗").red(), &device_id, matches.len());
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            device_id.clone()
                        };
                        rest.pair_revoke_device(&full_id).await?;
                        println!(
                            "{} Revoked device {}",
                            style("✓").green(),
                            &full_id[..8.min(full_id.len())]
                        );
                    }
                    PairCommand::Allowlist { platform, .. } => {
                        let resp = rest.pair_allowlist(platform.as_deref()).await?;
                        let entries = resp.as_array();
                        if entries.is_none_or(|e| e.is_empty()) {
                            println!("{} Allowlist is empty", style("ℹ").blue());
                        } else {
                            let entries = entries.unwrap();
                            println!("Channel allowlist ({} entries):", entries.len());
                            for entry in entries {
                                println!(
                                    "  [{}] {} ({})",
                                    entry["platform"].as_str().unwrap_or("?"),
                                    entry["sender_name"].as_str().unwrap_or("?"),
                                    entry["sender_id"].as_str().unwrap_or("?"),
                                );
                            }
                        }
                    }
                    // Allow, Revoke, Config are filtered out by supports_remote
                    _ => unreachable!(),
                }
            } else if matches!(command, PairCommand::Config) {
                // Config only reads workforce.toml — no DB needed
                let config = workforce_common::settings::WorkforceConfig::load(
                    &workdir.join("workforce.toml"),
                )
                .map(|c| c.pairing)
                .unwrap_or_default();

                println!("Pairing configuration:");
                println!(
                    "  Channel policy:   {}",
                    style(format!("{:?}", config.channel_policy)).cyan()
                );
                if !config.platform_policies.is_empty() {
                    println!("  Platform overrides:");
                    for (platform, policy) in &config.platform_policies {
                        println!(
                            "    {}: {}",
                            platform,
                            style(format!("{:?}", policy)).cyan()
                        );
                    }
                }
                println!("  Code TTL:         {}s", config.code_ttl_secs);
                println!("  Max pending:      {}", config.max_pending);
                println!(
                    "  Device pairing:   {}",
                    if config.device_pairing_enabled {
                        style("enabled").green()
                    } else {
                        style("disabled").dim()
                    }
                );
                println!(
                    "  Node pairing:     {}",
                    if config.node_pairing_enabled {
                        style("enabled").green()
                    } else {
                        style("disabled").dim()
                    }
                );
            } else {
                // ── Local mode: direct database access ──
                let pairing_db_path = data_dir.join("pairing.db");
                let store: Arc<dyn workforce_pairing::PairingStore> = Arc::new(
                    workforce_pairing::SqlitePairingStore::open(&pairing_db_path)?,
                );

                match command {
                    PairCommand::Pending { kind, .. } => {
                        let show_channel = kind.as_deref().is_none_or(|k| k == "channel");
                        let show_device = kind.as_deref().is_none_or(|k| k == "device");
                        let show_node = kind.as_deref().is_none_or(|k| k == "node");

                        if show_channel {
                            let reqs = store.list_pending_channel_requests()?;
                            if !reqs.is_empty() {
                                println!("Channel pairing requests:");
                                for req in &reqs {
                                    println!(
                                        "  {} [{}] {} ({}) — expires {}",
                                        style(&req.code).cyan().bold(),
                                        req.platform,
                                        req.sender_name,
                                        req.sender_id,
                                        req.expires_at.format("%H:%M:%S"),
                                    );
                                }
                            }
                        }
                        if show_device {
                            let reqs = store.list_pending_device_requests()?;
                            if !reqs.is_empty() {
                                println!("Device pairing requests:");
                                for req in &reqs {
                                    println!(
                                        "  {} {} (scopes: {}) — expires {}",
                                        style(&req.code).cyan().bold(),
                                        req.device_name,
                                        req.scopes.join(", "),
                                        req.expires_at.format("%H:%M:%S"),
                                    );
                                }
                            }
                        }
                        if show_node {
                            let reqs = store.list_pending_node_requests()?;
                            if !reqs.is_empty() {
                                println!("Node pairing requests:");
                                for req in &reqs {
                                    println!(
                                        "  {} {} [{}] ({}) — expires {}",
                                        style(&req.code).cyan().bold(),
                                        req.node_name,
                                        req.node_type,
                                        req.node_id,
                                        req.expires_at.format("%H:%M:%S"),
                                    );
                                }
                            }
                        }

                        let total = store.count_pending()?;
                        if total == 0 {
                            println!("{} No pending pairing requests", style("✓").green());
                        }
                    }
                    PairCommand::Approve { code, .. } => {
                        match store.get_any_request_by_code(&code)? {
                            Some(workforce_pairing::store::PairingRequestKind::Channel(_)) => {
                                let config = workforce_pairing::PairingConfig::default();
                                let svc = workforce_pairing::ChannelPairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                let approved = svc.approve_code(&code, "cli")?;
                                println!(
                                    "{} Approved channel pairing for {} ({}) on {}",
                                    style("✓").green(),
                                    approved.sender_name,
                                    approved.sender_id,
                                    approved.platform,
                                );
                            }
                            Some(workforce_pairing::store::PairingRequestKind::Device(_)) => {
                                let config = workforce_pairing::PairingConfig {
                                    device_pairing_enabled: true,
                                    ..Default::default()
                                };
                                let svc = workforce_pairing::DevicePairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                let (device, token) = svc.approve(&code, "cli")?;
                                println!(
                                    "{} Approved device pairing for {}",
                                    style("✓").green(),
                                    device.device_name,
                                );
                                println!("  Device ID: {}", device.id);
                                println!("  Token:     {}", style(&token).yellow());
                                println!(
                                    "  {}",
                                    style("Save this token — it won't be shown again.").dim()
                                );
                            }
                            Some(workforce_pairing::store::PairingRequestKind::Node(_)) => {
                                let config = workforce_pairing::PairingConfig::default();
                                let svc = workforce_pairing::NodePairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                let approved = svc.approve(&code, "cli")?;
                                println!(
                                    "{} Approved node pairing for {} ({})",
                                    style("✓").green(),
                                    approved.node_name,
                                    approved.node_id,
                                );
                            }
                            None => {
                                eprintln!("{} Unknown pairing code: {}", style("✗").red(), code);
                                std::process::exit(1);
                            }
                        }
                    }
                    PairCommand::Reject { code, .. } => {
                        match store.get_any_request_by_code(&code)? {
                            Some(workforce_pairing::store::PairingRequestKind::Channel(_)) => {
                                let config = workforce_pairing::PairingConfig::default();
                                let svc = workforce_pairing::ChannelPairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                svc.reject_code(&code)?;
                            }
                            Some(workforce_pairing::store::PairingRequestKind::Device(_)) => {
                                let config = workforce_pairing::PairingConfig {
                                    device_pairing_enabled: true,
                                    ..Default::default()
                                };
                                let svc = workforce_pairing::DevicePairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                svc.reject(&code)?;
                            }
                            Some(workforce_pairing::store::PairingRequestKind::Node(_)) => {
                                let config = workforce_pairing::PairingConfig::default();
                                let svc = workforce_pairing::NodePairingService::new(
                                    Arc::clone(&store),
                                    config,
                                );
                                svc.reject(&code)?;
                            }
                            None => {
                                eprintln!("{} Unknown pairing code: {}", style("✗").red(), code);
                                std::process::exit(1);
                            }
                        }
                        println!("{} Rejected pairing code {}", style("✓").green(), code);
                    }
                    PairCommand::Allowlist { platform, .. } => {
                        let entries = store.list_allowlist(platform.as_deref())?;
                        if entries.is_empty() {
                            println!("{} Allowlist is empty", style("ℹ").blue());
                        } else {
                            println!("Channel allowlist ({} entries):", entries.len());
                            for entry in &entries {
                                println!(
                                    "  [{}] {} ({}) — added {} by {}",
                                    entry.platform,
                                    entry.sender_name,
                                    entry.sender_id,
                                    entry.added_at.format("%Y-%m-%d"),
                                    entry.added_by,
                                );
                            }
                        }
                    }
                    PairCommand::Allow {
                        platform,
                        sender_id,
                        name,
                    } => {
                        let entry = workforce_pairing::ChannelAllowlistEntry {
                            id: uuid::Uuid::new_v4().to_string(),
                            platform: platform.clone(),
                            channel_id: "manual".to_string(),
                            sender_id: sender_id.clone(),
                            sender_name: name.unwrap_or_else(|| sender_id.clone()),
                            added_at: chrono::Utc::now(),
                            added_by: "manual".to_string(),
                        };
                        store.add_to_allowlist(&entry)?;
                        println!(
                            "{} Added {} to {} allowlist",
                            style("✓").green(),
                            sender_id,
                            platform,
                        );
                    }
                    PairCommand::Revoke {
                        platform,
                        sender_id,
                    } => {
                        if store.remove_from_allowlist(&platform, &sender_id)? {
                            println!(
                                "{} Removed {} from {} allowlist",
                                style("✓").green(),
                                sender_id,
                                platform,
                            );
                        } else {
                            eprintln!(
                                "{} {} not found in {} allowlist",
                                style("✗").red(),
                                sender_id,
                                platform,
                            );
                            std::process::exit(1);
                        }
                    }
                    PairCommand::Devices { .. } => {
                        let devices = store.list_devices()?;
                        if devices.is_empty() {
                            println!("{} No paired devices", style("ℹ").blue());
                        } else {
                            println!("Paired devices ({}):", devices.len());
                            for device in &devices {
                                let status = if device.revoked {
                                    style("revoked").red()
                                } else {
                                    style("active").green()
                                };
                                println!(
                                    "  {} {} [{}] (scopes: {}) — {}",
                                    style(&device.id[..8]).dim(),
                                    device.device_name,
                                    status,
                                    device.scopes.join(", "),
                                    device.created_at.format("%Y-%m-%d"),
                                );
                            }
                        }
                    }
                    PairCommand::RevokeDevice { device_id, .. } => {
                        // Resolve prefix to full ID
                        let full_id = if device_id.len() < 36 {
                            let devices = store.list_devices()?;
                            let matches: Vec<&str> = devices
                                .iter()
                                .map(|d| d.id.as_str())
                                .filter(|id| id.starts_with(&device_id))
                                .collect();
                            match matches.len() {
                                0 => {
                                    eprintln!(
                                        "{} No device matching '{}'",
                                        style("✗").red(),
                                        &device_id
                                    );
                                    std::process::exit(1);
                                }
                                1 => matches[0].to_string(),
                                _ => {
                                    eprintln!("{} Ambiguous prefix '{}' — matches {} devices. Use a longer prefix.", style("✗").red(), &device_id, matches.len());
                                    std::process::exit(1);
                                }
                            }
                        } else {
                            device_id.clone()
                        };
                        store.revoke_device(&full_id)?;
                        println!(
                            "{} Revoked device {}",
                            style("✓").green(),
                            &full_id[..8.min(full_id.len())]
                        );
                    }
                    // Config and Request are handled above
                    PairCommand::Config | PairCommand::Request { .. } => unreachable!(),
                }
            }
        }
        Commands::Policy { command, data_dir } => {
            use workforce_common::persistence::SqliteStore;

            std::fs::create_dir_all(&data_dir)?;
            let db_path = data_dir.join("telemetry.db");
            let store = SqliteStore::open(&db_path)?;

            match command {
                PolicyCommand::List { status, limit } => {
                    let proposals = store.list_policy_proposals(status.as_deref(), limit)?;

                    if proposals.is_empty() {
                        println!("{}", style("No policy proposals found.").dim());
                    } else {
                        println!(
                            "{:<4} {:<10} {:<12} {:<20} {}",
                            style("ID").bold(),
                            style("Status").bold(),
                            style("Agent").bold(),
                            style("Created").bold(),
                            style("Justification").bold(),
                        );
                        for p in &proposals {
                            let status_styled = match p.status.as_str() {
                                "pending" => style(&p.status).yellow(),
                                "approved" => style(&p.status).green(),
                                "rejected" => style(&p.status).red(),
                                _ => style(&p.status).dim(),
                            };
                            let created_short = p.created_at.get(..19).unwrap_or(&p.created_at);
                            let justification_short: String =
                                p.justification.chars().take(50).collect();
                            println!(
                                "{:<4} {:<10} {:<12} {:<20} {}",
                                p.id, status_styled, p.agent_id, created_short, justification_short,
                            );
                        }
                        println!("\n  {} proposals shown.", style(proposals.len()).bold());
                    }
                }
                PolicyCommand::Show { id } => {
                    match store.get_policy_proposal(id)? {
                        Some(p) => {
                            println!(
                                "{} Policy Proposal #{}",
                                style("📋").bold(),
                                style(id).cyan()
                            );
                            println!(
                                "  Status:        {}",
                                match p.status.as_str() {
                                    "pending" => style(&p.status).yellow().to_string(),
                                    "approved" => style(&p.status).green().to_string(),
                                    "rejected" => style(&p.status).red().to_string(),
                                    _ => p.status.clone(),
                                }
                            );
                            println!("  Agent:         {}", p.agent_id);
                            println!("  Created:       {}", p.created_at);
                            println!("  Justification: {}", p.justification);
                            if let Some(ref by) = p.reviewed_by {
                                println!("  Reviewed by:   {}", by);
                            }
                            if let Some(ref note) = p.review_note {
                                println!("  Review note:   {}", note);
                            }
                            if let Some(ref at) = p.reviewed_at {
                                println!("  Reviewed at:   {}", at);
                            }
                            // Pretty-print the rule
                            if let Ok(rule) =
                                serde_json::from_str::<serde_json::Value>(&p.proposed_rule)
                            {
                                println!("  Rule:");
                                println!(
                                    "    {}",
                                    serde_json::to_string_pretty(&rule)
                                        .unwrap_or(p.proposed_rule.clone())
                                );
                            } else {
                                println!("  Rule (raw): {}", p.proposed_rule);
                            }
                        }
                        None => {
                            println!("{} Proposal #{} not found.", style("✗").red(), id);
                        }
                    }
                }
                PolicyCommand::Approve { id, note } => match store.get_policy_proposal(id)? {
                    Some(p) if p.status != "pending" => {
                        println!(
                            "{} Proposal #{} is already {}.",
                            style("✗").red(),
                            id,
                            p.status
                        );
                    }
                    Some(_) => {
                        let reviewer =
                            std::env::var("USER").unwrap_or_else(|_| "cli-user".to_string());
                        store.review_policy_proposal(id, "approved", &reviewer, note.as_deref())?;
                        println!(
                            "{} Proposal #{} approved by {}.",
                            style("✓").green(),
                            style(id).cyan(),
                            reviewer,
                        );
                        println!(
                            "  {} The rule will be loaded on next agent restart or hub reload.",
                            style("Note:").dim()
                        );
                    }
                    None => {
                        println!("{} Proposal #{} not found.", style("✗").red(), id);
                    }
                },
                PolicyCommand::Reject { id, note } => match store.get_policy_proposal(id)? {
                    Some(p) if p.status != "pending" => {
                        println!(
                            "{} Proposal #{} is already {}.",
                            style("✗").red(),
                            id,
                            p.status
                        );
                    }
                    Some(_) => {
                        let reviewer =
                            std::env::var("USER").unwrap_or_else(|_| "cli-user".to_string());
                        store.review_policy_proposal(id, "rejected", &reviewer, note.as_deref())?;
                        println!(
                            "{} Proposal #{} rejected by {}.",
                            style("✗").red(),
                            style(id).cyan(),
                            reviewer,
                        );
                    }
                    None => {
                        println!("{} Proposal #{} not found.", style("✗").red(), id);
                    }
                },
            }
        }
        Commands::Role { command, config } => {
            use workforce_common::roles;
            use workforce_common::settings::WorkforceConfig;

            let data_dir = if config.exists() {
                let wf_config = WorkforceConfig::load(&config)?;
                wf_config.hub.data_dir.clone()
            } else {
                "./data".to_string()
            };

            match command {
                RoleCommand::List => {
                    let roles = roles::list_roles(&data_dir)?;
                    if roles.is_empty() {
                        println!(
                            "No role templates found. Run `workforce role init` to create built-in roles."
                        );
                    } else {
                        println!("{} {} role template(s):\n", style("📋").bold(), roles.len());
                        for r in &roles {
                            println!(
                                "  {} {} — {}",
                                style("•").cyan(),
                                style(&r.id).bold(),
                                r.name,
                            );
                            if !r.capabilities.is_empty() {
                                println!(
                                    "    Capabilities: {}",
                                    style(r.capabilities.join(", ")).dim()
                                );
                            }
                            if !r.expertise.is_empty() {
                                println!(
                                    "    Expertise:    {}",
                                    style(r.expertise.join(", ")).dim()
                                );
                            }
                        }
                    }
                }
                RoleCommand::Show { id } => {
                    let role = roles::load_role(&data_dir, &id)?;
                    println!("{} Role: {}", style("📋").bold(), style(&role.name).bold());
                    println!("  ID: {}", role.id);
                    if let Some(ref cs) = role.communication_style {
                        println!("  Style: {}", cs);
                    }
                    if !role.capabilities.is_empty() {
                        println!("  Capabilities: {}", role.capabilities.join(", "));
                    }
                    if !role.expertise.is_empty() {
                        println!("  Expertise: {}", role.expertise.join(", "));
                    }
                    if let Some(ref p) = role.personality {
                        println!("  Personality:");
                        if let Some(v) = p.assertiveness {
                            println!("    Assertiveness:      {:.1}", v);
                        }
                        if let Some(v) = p.curiosity {
                            println!("    Curiosity:          {:.1}", v);
                        }
                        if let Some(v) = p.pragmatism {
                            println!("    Pragmatism:         {:.1}", v);
                        }
                        if let Some(v) = p.detail_orientation {
                            println!("    Detail orientation: {:.1}", v);
                        }
                    }
                    println!("\n  System prompt:\n");
                    for line in role.system_prompt.lines() {
                        println!("    {}", style(line).dim());
                    }
                }
                RoleCommand::Create {
                    id,
                    name,
                    prompt,
                    capabilities,
                    expertise,
                    style: comm_style,
                } => {
                    // Check if role already exists
                    if roles::load_role(&data_dir, &id).is_ok() {
                        return Err(eyre::eyre!(
                            "Role '{}' already exists. Delete it first to recreate.",
                            id
                        ));
                    }

                    let role = roles::RoleTemplate {
                        id: id.clone(),
                        name,
                        system_prompt: prompt,
                        capabilities: capabilities.unwrap_or_default(),
                        expertise: expertise.unwrap_or_default(),
                        communication_style: comm_style,
                        personality: None,
                    };
                    roles::save_role(&data_dir, &role)?;
                    println!(
                        "{} Created role template '{}'",
                        style("✓").green().bold(),
                        style(&id).bold()
                    );
                }
                RoleCommand::Delete { id } => {
                    roles::delete_role(&data_dir, &id)?;
                    println!(
                        "{} Deleted role template '{}'",
                        style("✓").green().bold(),
                        style(&id).bold()
                    );
                }
                RoleCommand::Init => {
                    let created = roles::init_builtin_roles(&data_dir)?;
                    if created.is_empty() {
                        println!("All built-in role templates already exist.");
                    } else {
                        println!(
                            "{} Created {} built-in role template(s):",
                            style("✓").green().bold(),
                            created.len()
                        );
                        for id in &created {
                            println!("  {} {}", style("•").cyan(), id);
                        }
                    }
                }
            }
        }
        Commands::Slack { command, config } => match command {
            SlackCommand::Scaffold { name, bot_name } => {
                let bot = bot_name.as_deref().unwrap_or(&name);
                let manifest = workforce_hub::slack_tool::generate_manifest(&name, bot);
                println!(
                    "{} Slack App Manifest for '{}':\n",
                    style("📱").bold(),
                    style(&name).bold()
                );
                println!("{}", manifest);
                println!("Steps:");
                println!("  1. Go to https://api.slack.com/apps");
                println!("  2. Click 'Create New App' → 'From an app manifest'");
                println!("  3. Select your workspace");
                println!("  4. Paste the manifest above (switch to YAML mode)");
                println!("  5. Install the app to your workspace");
                println!(
                    "  6. Run: workforce slack configure --bot-token <token> --app-token <token>"
                );
            }
            SlackCommand::Configure {
                workspace_id,
                bot_token,
                signing_secret,
                app_token,
            } => {
                use workforce_common::settings::WorkforceConfig;
                let wf_config = if config.exists() {
                    WorkforceConfig::load(&config)?
                } else {
                    return Err(eyre::eyre!("Config not found. Run `workforce init` first."));
                };
                let vault = workforce_hub::vault::open_global(&wf_config)
                    .await?
                    .ok_or_else(|| eyre::eyre!("the vault is disabled or could not be opened"))?;
                let actor = workforce_common::settings::VaultActor::Cli;
                let prefix = format!("slack.{}.", workspace_id);
                let key = format!("{}bot_token", prefix);
                vault.set_with_actor(&key, &bot_token, actor)?;
                println!("{} Stored {}", style("✓").green().bold(), key);
                if let Some(ref ss) = signing_secret {
                    let key = format!("{}signing_secret", prefix);
                    vault.set_with_actor(&key, ss, actor)?;
                    println!("{} Stored {}", style("✓").green().bold(), key);
                }
                if let Some(ref at) = app_token {
                    let key = format!("{}app_token", prefix);
                    vault.set_with_actor(&key, at, actor)?;
                    println!("{} Stored {}", style("✓").green().bold(), key);
                }
            }
        },
        Commands::Google { command, config } => {
            use workforce_common::settings::WorkforceConfig;

            let wf_config = if config.exists() {
                WorkforceConfig::load(&config)?
            } else {
                return Err(eyre::eyre!("Config not found. Run `workforce init` first."));
            };
            let vault = workforce_hub::vault::open_global(&wf_config)
                .await?
                .ok_or_else(|| eyre::eyre!("the vault is disabled or could not be opened"))?;

            match command {
                GoogleCommand::Login {
                    workspace,
                    no_browser,
                    client_id,
                    client_secret,
                } => {
                    let override_client = match (client_id, client_secret) {
                        (Some(id), Some(secret)) => Some(
                            workforce_skills::adapters::google::oauth::OAuthClientCreds {
                                client_id: id,
                                client_secret: secret,
                                redirect_uri: None,
                            },
                        ),
                        _ => None,
                    };
                    let opts = workforce_skills::adapters::google::oauth::LoginOptions {
                        workspace: workspace.clone(),
                        no_browser,
                        override_client,
                    };
                    let store =
                        vault as std::sync::Arc<dyn workforce_common::settings::SecretStore>;
                    let tokens =
                        workforce_skills::adapters::google::oauth::run_login(store, opts).await?;
                    println!(
                        "{} Logged in as {} (workspace '{}')",
                        style("✓").green().bold(),
                        style(tokens.email.as_deref().unwrap_or("unknown")).bold(),
                        style(&workspace).cyan(),
                    );
                }
                GoogleCommand::Logout { workspace } => {
                    workforce_skills::adapters::google::oauth::delete_workspace(
                        vault.as_ref() as &dyn workforce_common::settings::SecretStore,
                        &workspace,
                    )?;
                    println!(
                        "{} Logged out of workspace '{}'",
                        style("✓").green().bold(),
                        style(&workspace).cyan(),
                    );
                }
                GoogleCommand::List => {
                    let store: &dyn workforce_common::settings::SecretStore = vault.as_ref();
                    let workspaces =
                        workforce_skills::adapters::google::token_store::list_workspaces(store);
                    if workspaces.is_empty() {
                        println!("No Google accounts configured.");
                        println!("  Run: workforce google login --workspace <name>");
                    } else {
                        println!(
                            "{} {} Google account(s):\n",
                            style("🔐").bold(),
                            workspaces.len()
                        );
                        for ws in &workspaces {
                            let email = store
                                .get(&format!("skills/google/workspaces/{ws}/email"))
                                .unwrap_or_else(|| "(unknown)".to_string());
                            println!("  {} {} — {}", style("•").cyan(), style(ws).bold(), email);
                        }
                    }
                }
            }
        }
        Commands::Agent { command, config } => {
            use workforce_common::settings::WorkforceConfig;

            let wf_config = if config.exists() {
                WorkforceConfig::load(&config)?
            } else {
                return Err(eyre::eyre!(
                    "Config file '{}' not found. Run `workforce init` first.",
                    config.display()
                ));
            };

            match command {
                AgentCommand::List => {
                    if wf_config.agents.is_empty() {
                        println!("No agents configured. Add [[agents]] to workforce.toml or use `workforce agent create`.");
                    } else {
                        println!(
                            "{} {} agent(s) configured:\n",
                            style("🤖").bold(),
                            wf_config.agents.len()
                        );
                        for a in &wf_config.agents {
                            let name = a.name.as_deref().unwrap_or(&a.id);
                            let model = a.model.as_ref().unwrap_or(&wf_config.llm.default_model);
                            let default_marker = if a.default { " (default)" } else { "" };
                            println!(
                                "  {} {} — model: {}{}",
                                style("•").cyan(),
                                style(name).bold(),
                                style(model).dim(),
                                style(default_marker).green()
                            );
                            if !a.capabilities.is_empty() {
                                println!(
                                    "    Capabilities: {}",
                                    style(a.capabilities.join(", ")).dim()
                                );
                            }
                        }
                    }
                }
                AgentCommand::Show { id } => {
                    let agent = wf_config
                        .agents
                        .iter()
                        .find(|a| a.id == id)
                        .ok_or_else(|| eyre::eyre!("Agent '{}' not found in config", id))?;
                    println!(
                        "{} Agent: {}",
                        style("🤖").bold(),
                        style(agent.name.as_deref().unwrap_or(&agent.id)).bold()
                    );
                    println!("  ID:       {}", agent.id);
                    if let Some(ref m) = agent.model {
                        println!("  Model:    {}", m);
                    }
                    if let Some(ref ws) = agent.workspace {
                        println!("  Workspace: {}", ws);
                    }
                    println!("  Default:  {}", agent.default);
                    if !agent.capabilities.is_empty() {
                        println!("  Capabilities: {}", agent.capabilities.join(", "));
                    }
                    if let Some(ref sp) = agent.system_prompt {
                        println!("\n  System prompt:\n");
                        for line in sp.lines() {
                            println!("    {}", style(line).dim());
                        }
                    }
                }
                AgentCommand::Create {
                    id,
                    name,
                    role,

                    prompt,
                    capabilities,
                    default,
                } => {
                    let config_mut = wf_config.clone();

                    // Check for duplicate ID
                    if config_mut.agents.iter().any(|a| a.id == id) {
                        return Err(eyre::eyre!("Agent '{}' already exists", id));
                    }

                    // Load role template if specified
                    let role_template = if let Some(ref role_id) = role {
                        Some(workforce_common::roles::load_role(
                            &config_mut.hub.data_dir,
                            role_id,
                        )?)
                    } else {
                        None
                    };

                    // Build agent config (role provides defaults, CLI args override)
                    let system_prompt =
                        prompt.or_else(|| role_template.as_ref().map(|r| r.system_prompt.clone()));
                    let caps = capabilities.unwrap_or_else(|| {
                        role_template
                            .as_ref()
                            .map(|r| r.capabilities.clone())
                            .unwrap_or_default()
                    });

                    let workspace_path = format!("{}/agents/{}", config_mut.hub.data_dir, id);

                    // Raw overlay entry — runtime agent creation writes the
                    // overlay ({data_dir}/workforce.overlay.toml), never the
                    // infra-owned workforce.toml.
                    let mut agent_entry = toml::Table::new();
                    agent_entry.insert("id".to_string(), toml::Value::String(id.clone()));
                    if let Some(n) = name.or_else(|| role_template.as_ref().map(|r| r.name.clone()))
                    {
                        agent_entry.insert("name".to_string(), toml::Value::String(n));
                    }
                    agent_entry.insert(
                        "workspace".to_string(),
                        toml::Value::String(workspace_path.clone()),
                    );
                    if let Some(p) = system_prompt {
                        agent_entry.insert("system_prompt".to_string(), toml::Value::String(p));
                    }
                    if !caps.is_empty() {
                        agent_entry.insert(
                            "capabilities".to_string(),
                            toml::Value::Array(caps.into_iter().map(toml::Value::String).collect()),
                        );
                    }
                    if default {
                        agent_entry.insert("default".to_string(), toml::Value::Boolean(true));
                    }

                    // Create workspace directory and identity files
                    let ws_path = std::path::Path::new(&workspace_path);
                    let created_files = write_identity_files(ws_path)?;

                    // If role has a system prompt, write it as SOUL.md
                    if let Some(ref rt) = role_template {
                        let soul_path = ws_path.join("SOUL.md");
                        let soul_content =
                            format!("# SOUL.md — {}\n\n{}\n", rt.name, rt.system_prompt);
                        std::fs::write(&soul_path, soul_content)?;
                    }

                    let lock_manager = workforce_common::locking::LockManager::new();
                    let def_store = workforce_hub::serve::shared_workflow_store(&config_mut).await;
                    workforce_hub::config_writer::overlay_upsert_agent(
                        &config,
                        std::path::Path::new(&config_mut.hub.data_dir),
                        agent_entry,
                        None,
                        "cli",
                        def_store.as_deref(),
                        &lock_manager,
                    )
                    .await?;

                    println!(
                        "{} Created agent '{}' (config overlay; restart the hub to load it)",
                        style("✓").green().bold(),
                        style(&id).bold()
                    );
                    if role.is_some() {
                        println!("  Role: {}", style(role.as_deref().unwrap()).dim());
                    }
                    println!("  Workspace: {}", style(&workspace_path).dim());
                    println!("  Files created: {}", created_files.join(", "));
                }
                AgentCommand::Update {
                    id,
                    name,

                    prompt,
                    default,
                } => {
                    // Update the agent's RAW entry (overlay/base tables, never
                    // the env-expanded struct) and write it to the overlay.
                    let lock_manager = workforce_common::locking::LockManager::new();
                    let def_store = workforce_hub::serve::shared_workflow_store(&wf_config).await;
                    workforce_hub::config_writer::overlay_update_agent_fields(
                        &config,
                        std::path::Path::new(&wf_config.hub.data_dir),
                        &id,
                        move |entry| {
                            if let Some(n) = name {
                                entry.insert("name".to_string(), toml::Value::String(n));
                            }
                            if let Some(p) = prompt {
                                entry.insert("system_prompt".to_string(), toml::Value::String(p));
                            }
                            if let Some(d) = default {
                                entry.insert("default".to_string(), toml::Value::Boolean(d));
                            }
                        },
                        None,
                        "cli",
                        def_store.as_deref(),
                        &lock_manager,
                    )
                    .await?;

                    println!(
                        "{} Updated agent '{}' (config overlay; restart the hub to apply)",
                        style("✓").green().bold(),
                        style(&id).bold()
                    );
                }
                AgentCommand::Delete {
                    id,
                    delete_workspace,
                } => {
                    let removed = wf_config
                        .agents
                        .iter()
                        .find(|a| a.id == id)
                        .cloned()
                        .ok_or_else(|| eyre::eyre!("Agent '{}' not found", id))?;

                    if delete_workspace {
                        if let Some(ref ws) = removed.workspace {
                            let ws_path = std::path::Path::new(ws);
                            if ws_path.exists() {
                                std::fs::remove_dir_all(ws_path)?;
                                println!("  Deleted workspace: {}", ws);
                            }
                        }
                    }

                    // Drop any overlay entry + tombstone the id so a
                    // base-defined agent stays gone across infra deploys.
                    let lock_manager = workforce_common::locking::LockManager::new();
                    let def_store = workforce_hub::serve::shared_workflow_store(&wf_config).await;
                    workforce_hub::config_writer::overlay_remove_agent(
                        &config,
                        std::path::Path::new(&wf_config.hub.data_dir),
                        &id,
                        None,
                        "cli",
                        def_store.as_deref(),
                        &lock_manager,
                    )
                    .await?;

                    println!(
                        "{} Deleted agent '{}' (config overlay; restart the hub to apply)",
                        style("✓").green().bold(),
                        style(&id).bold()
                    );
                }
            }
        }
        Commands::Heartbeat { command, config } => {
            use workforce_common::heartbeat_check;
            use workforce_common::settings::WorkforceConfig;

            let wf_config = if config.exists() {
                WorkforceConfig::load(&config)?
            } else {
                return Err(eyre::eyre!(
                    "Config file '{}' not found. Run `workforce init` first.",
                    config.display()
                ));
            };

            // Resolve workspace path for agent
            let resolve_workspace =
                |agent: &str| -> PathBuf {
                    if agent == "default" {
                        PathBuf::from(&wf_config.hub.data_dir).join("workspace")
                    } else {
                        wf_config
                            .agents
                            .iter()
                            .find(|a| a.id == agent)
                            .map(|a| {
                                PathBuf::from(a.workspace.as_deref().unwrap_or(&format!(
                                    "{}/agents/{}",
                                    wf_config.hub.data_dir, a.id
                                )))
                            })
                            .unwrap_or_else(|| {
                                PathBuf::from(&wf_config.hub.data_dir)
                                    .join("agents")
                                    .join(agent)
                            })
                    }
                };

            match command {
                HeartbeatCommand::List { agent } => {
                    let workspace = resolve_workspace(&agent);
                    let checks = heartbeat_check::list_heartbeat_checks(&workspace)?;
                    if checks.is_empty() {
                        println!(
                            "No heartbeat checks found for agent '{}'. Run `workforce heartbeat init {}` to create templates.",
                            agent, agent
                        );
                    } else {
                        println!(
                            "{} {} heartbeat check(s) for agent '{}':\n",
                            style("♥").bold(),
                            checks.len(),
                            agent
                        );
                        for c in &checks {
                            let schedule = c.meta.schedule.as_deref().unwrap_or("(interval)");
                            println!(
                                "  {} {} [p{}] {} ({})",
                                if c.meta.enabled {
                                    style("●").green().to_string()
                                } else {
                                    style("○").dim().to_string()
                                },
                                style(&c.id).bold(),
                                c.meta.priority,
                                c.meta.name,
                                style(schedule).dim()
                            );
                        }
                    }
                }
                HeartbeatCommand::Show { agent, name } => {
                    let workspace = resolve_workspace(&agent);
                    let check = heartbeat_check::load_heartbeat_check(&workspace, &name)?;
                    println!(
                        "{} Heartbeat: {}\n",
                        style("♥").bold(),
                        style(&check.meta.name).bold()
                    );
                    println!("  ID:       {}", check.id);
                    println!("  Enabled:  {}", check.meta.enabled);
                    println!(
                        "  Schedule: {}",
                        check
                            .meta
                            .schedule
                            .as_deref()
                            .unwrap_or("(global interval)")
                    );
                    println!("  Priority: {}", check.meta.priority);
                    if !check.meta.tags.is_empty() {
                        println!("  Tags:     {}", check.meta.tags.join(", "));
                    }
                    println!("\n{}", check.body);
                }
                HeartbeatCommand::Create {
                    agent,
                    name,
                    display_name,
                    schedule,
                    priority,
                    body,
                } => {
                    let workspace = resolve_workspace(&agent);
                    let check = heartbeat_check::HeartbeatCheck {
                        id: name.clone(),
                        meta: heartbeat_check::HeartbeatCheckMeta {
                            name: display_name.unwrap_or_else(|| name.replace(['-', '_'], " ")),
                            enabled: true,
                            schedule,
                            priority,
                            tags: vec![],
                            timezone: None,
                            cooldown_secs: None,
                            take_action: false,
                            tools: None,
                            creator: None,
                            creator_sig: None,
                        },
                        body: body.unwrap_or_else(|| {
                            format!(
                                "# {}\n\n## What to check:\n- Add your checks here\n\n## Output:\n\
                                 - If nothing needs attention: reply HEARTBEAT_OK",
                                name
                            )
                        }),
                    };
                    heartbeat_check::save_heartbeat_check(&workspace, &check)?;
                    println!(
                        "{} Created heartbeat check '{}' for agent '{}'",
                        style("✓").green().bold(),
                        style(&name).bold(),
                        agent
                    );
                }
                HeartbeatCommand::Edit {
                    agent,
                    name,
                    schedule,
                    priority,
                    enabled,
                    body,
                } => {
                    let workspace = resolve_workspace(&agent);
                    let mut check = heartbeat_check::load_heartbeat_check(&workspace, &name)?;
                    if let Some(s) = schedule {
                        check.meta.schedule = Some(s);
                    }
                    if let Some(p) = priority {
                        check.meta.priority = p;
                    }
                    if let Some(e) = enabled {
                        check.meta.enabled = e;
                    }
                    if let Some(b) = body {
                        check.body = b;
                    }
                    heartbeat_check::save_heartbeat_check(&workspace, &check)?;
                    println!(
                        "{} Updated heartbeat check '{}' for agent '{}'",
                        style("✓").green().bold(),
                        style(&name).bold(),
                        agent
                    );
                }
                HeartbeatCommand::Enable { agent, name } => {
                    let workspace = resolve_workspace(&agent);
                    let mut check = heartbeat_check::load_heartbeat_check(&workspace, &name)?;
                    check.meta.enabled = true;
                    heartbeat_check::save_heartbeat_check(&workspace, &check)?;
                    println!(
                        "{} Enabled heartbeat check '{}' for agent '{}'",
                        style("✓").green().bold(),
                        style(&name).bold(),
                        agent
                    );
                }
                HeartbeatCommand::Disable { agent, name } => {
                    let workspace = resolve_workspace(&agent);
                    let mut check = heartbeat_check::load_heartbeat_check(&workspace, &name)?;
                    check.meta.enabled = false;
                    heartbeat_check::save_heartbeat_check(&workspace, &check)?;
                    println!(
                        "{} Disabled heartbeat check '{}' for agent '{}'",
                        style("✓").green().bold(),
                        style(&name).bold(),
                        agent
                    );
                }
                HeartbeatCommand::Delete { agent, name } => {
                    let workspace = resolve_workspace(&agent);
                    heartbeat_check::delete_heartbeat_check(&workspace, &name)?;
                    println!(
                        "{} Deleted heartbeat check '{}' for agent '{}'",
                        style("✓").green().bold(),
                        style(&name).bold(),
                        agent
                    );
                }
                HeartbeatCommand::Init { agent } => {
                    let workspace = resolve_workspace(&agent);
                    let created = heartbeat_check::init_builtin_checks(&workspace)?;
                    if created.is_empty() {
                        println!(
                            "All built-in heartbeat checks already exist for agent '{}'.",
                            agent
                        );
                    } else {
                        println!(
                            "{} Created {} built-in heartbeat check(s) for agent '{}':",
                            style("✓").green().bold(),
                            created.len(),
                            agent
                        );
                        for id in &created {
                            println!("  {} {}", style("+").cyan(), id);
                        }
                    }
                }
            }
        }
        Commands::Lessons { command } => match command {
            LessonsCommand::Backfill {
                config,
                agent,
                since,
                dry_run,
                model,
                print_lessons,
            } => {
                use workforce_common::settings::WorkforceConfig;

                let wf_config = WorkforceConfig::load(&config)
                    .map_err(|e| eyre::eyre!("Loading {}: {}", config.display(), e))?;
                let data_dir = PathBuf::from(&wf_config.hub.data_dir);

                let (provider_base, default_tier_key) =
                    workforce_agent::llm::tiers::parse_provider_tier_key(&wf_config.llm.provider)
                        .map_err(|e| {
                        eyre::eyre!(
                            "Failed to parse provider '{}': {}",
                            wf_config.llm.provider,
                            e
                        )
                    })?;
                let (provider_kind, api_key) = resolve_provider_and_key(&provider_base, None)?;
                let thinking_budget = wf_config.llm.thinking_budget_tokens;
                let zai_sub = wf_config.llm.zai_subscription;
                let base_llm =
                    create_provider_async(provider_kind, &api_key, None, thinking_budget, zai_sub)
                        .await?;
                let llm: Arc<dyn LlmProvider> =
                    Arc::from(wrap_with_failover(base_llm, Some(&wf_config)));

                let resolved_model = resolve_extract_model(
                    model.as_deref(),
                    &wf_config.llm.default_model,
                    &default_tier_key,
                );

                let store: Arc<dyn workforce_memory::store::MemoryStore> = if dry_run {
                    Arc::new(workforce_memory::in_memory::InMemoryStore::new())
                } else {
                    workforce_hub::state::resolve_memory_store_with_budget(
                        &wf_config,
                        CLI_MEMORY_CONNECT_TIMEOUT,
                    )
                    .await?
                };

                let agent_ids: Vec<AgentId> = agent.iter().map(AgentId::new).collect();
                let scope_label = if agent_ids.is_empty() {
                    "all agents".to_string()
                } else {
                    agent_ids
                        .iter()
                        .map(|a| a.as_str().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                eprintln!(
                    "{} Lessons backfill: scope={} since={} dry_run={}",
                    style("📚").bold(),
                    style(scope_label).cyan(),
                    since
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| "(all history)".to_string()),
                    dry_run,
                );

                let opts = workforce_hub::lessons_backfill::BackfillOptions {
                    agent_ids,
                    since,
                    dry_run,
                    model: resolved_model,
                    print_lessons,
                };
                let report =
                    workforce_hub::lessons_backfill::run(&data_dir, llm, store, opts).await?;
                println!(
                    "{}",
                    workforce_hub::lessons_backfill::format_report(&report)
                );
                workforce_hub::lessons_backfill::log_report_summary(&report);
            }
            LessonsCommand::Cleanup {
                config,
                agent,
                commit,
                model,
                merge_similarity,
                print,
            } => {
                use workforce_common::settings::WorkforceConfig;

                let wf_config = WorkforceConfig::load(&config)
                    .map_err(|e| eyre::eyre!("Loading {}: {}", config.display(), e))?;

                let (provider_base, default_tier_key) =
                    workforce_agent::llm::tiers::parse_provider_tier_key(&wf_config.llm.provider)
                        .map_err(|e| {
                        eyre::eyre!(
                            "Failed to parse provider '{}': {}",
                            wf_config.llm.provider,
                            e
                        )
                    })?;
                let (provider_kind, api_key) = resolve_provider_and_key(&provider_base, None)?;
                let base_llm = create_provider_async(
                    provider_kind,
                    &api_key,
                    None,
                    wf_config.llm.thinking_budget_tokens,
                    wf_config.llm.zai_subscription,
                )
                .await?;
                let llm: Arc<dyn LlmProvider> =
                    Arc::from(wrap_with_failover(base_llm, Some(&wf_config)));
                let resolved_model = resolve_extract_model(
                    model.as_deref(),
                    &wf_config.llm.default_model,
                    &default_tier_key,
                );

                // Always the real store: cleanup reads the live corpus and a
                // dry run is expressed by not mutating, not by a throwaway store.
                let store: Arc<dyn workforce_memory::store::MemoryStore> =
                    workforce_hub::state::resolve_memory_store_with_budget(
                        &wf_config,
                        CLI_MEMORY_CONNECT_TIMEOUT,
                    )
                    .await?;

                let agent_ids: Vec<AgentId> = agent.iter().map(AgentId::new).collect();
                let mut opts = workforce_hub::lessons_cleanup::CleanupOptions {
                    agent_ids,
                    dry_run: !commit,
                    model: resolved_model,
                    print,
                    ..Default::default()
                };
                if let Some(sim) = merge_similarity {
                    opts.merge_similarity = sim;
                }
                eprintln!(
                    "{} Lessons cleanup: {} (merge≥{:.2})",
                    style("🧹").bold(),
                    if commit {
                        style("COMMIT — mutating store").red()
                    } else {
                        style("dry run — read only").cyan()
                    },
                    opts.merge_similarity,
                );

                let report = workforce_hub::lessons_cleanup::run(llm, store, opts).await?;
                println!("{}", workforce_hub::lessons_cleanup::format_report(&report));
                workforce_hub::lessons_cleanup::log_report_summary(&report);
            }
        },
        Commands::Audit { config, data_dir } => {
            println!("{} Running security audit...\n", style("🔒").bold());

            let mut findings: Vec<(&str, &str, String)> = Vec::new(); // (severity, check_id, detail)
            let mut critical = 0u32;
            let mut warnings = 0u32;
            let mut info_count = 0u32;

            // 1. Config file permissions
            if config.exists() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(&config)?;
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        findings.push(("CRITICAL", "config.perms", format!(
                            "Config file {} has permissions {:o} — should be 600 (readable only by owner)",
                            config.display(), mode
                        )));
                        critical += 1;
                    } else {
                        findings.push((
                            "OK",
                            "config.perms",
                            format!("Config file permissions: {:o}", mode),
                        ));
                        info_count += 1;
                    }
                }
                #[cfg(not(unix))]
                {
                    findings.push((
                        "INFO",
                        "config.perms",
                        "Permission checks not available on this platform".to_string(),
                    ));
                    info_count += 1;
                }
            } else {
                findings.push((
                    "INFO",
                    "config.missing",
                    format!("Config file {} not found", config.display()),
                ));
                info_count += 1;
            }

            // 2. Data directory permissions
            if data_dir.exists() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(&data_dir)?;
                    let mode = meta.permissions().mode() & 0o777;
                    if mode & 0o077 != 0 {
                        findings.push((
                            "WARN",
                            "data_dir.perms",
                            format!(
                                "Data directory {} has permissions {:o} — should be 700",
                                data_dir.display(),
                                mode
                            ),
                        ));
                        warnings += 1;
                    } else {
                        findings.push((
                            "OK",
                            "data_dir.perms",
                            format!("Data directory permissions: {:o}", mode),
                        ));
                        info_count += 1;
                    }
                }
                #[cfg(not(unix))]
                {
                    findings.push((
                        "INFO",
                        "data_dir.perms",
                        "Permission checks not available on this platform".to_string(),
                    ));
                    info_count += 1;
                }
            }

            // 3. Config content checks
            if config.exists() {
                let config_content = std::fs::read_to_string(&config).unwrap_or_default();

                // Check gateway token length
                if let Ok(cfg) = workforce_common::settings::WorkforceConfig::load(&config) {
                    if let Some(ref token) = cfg.gateway.auth_token {
                        if token.len() < 24 {
                            findings.push((
                                "WARN",
                                "gateway.token_short",
                                format!(
                                    "Gateway token is only {} characters — recommend 24+ chars",
                                    token.len()
                                ),
                            ));
                            warnings += 1;
                        } else {
                            findings.push((
                                "OK",
                                "gateway.token_length",
                                "Gateway token length adequate".to_string(),
                            ));
                            info_count += 1;
                        }
                    } else {
                        findings.push((
                            "WARN",
                            "gateway.no_token",
                            "No gateway auth token configured".to_string(),
                        ));
                        warnings += 1;
                    }
                }

                // Check for secrets in config
                if workforce_common::redact::contains_secret(&config_content) {
                    findings.push(("CRITICAL", "config.secrets", "Config file appears to contain API keys or tokens — use env vars or vault instead".to_string()));
                    critical += 1;
                } else {
                    findings.push((
                        "OK",
                        "config.no_secrets",
                        "No secrets detected in config file".to_string(),
                    ));
                    info_count += 1;
                }
            }

            // 4. Check for symlink attacks on config/data
            if config.exists() && config.is_symlink() {
                findings.push((
                    "WARN",
                    "config.symlink",
                    "Config file is a symlink — verify it points to a trusted location".to_string(),
                ));
                warnings += 1;
            }
            if data_dir.exists() && data_dir.is_symlink() {
                findings.push((
                    "WARN",
                    "data_dir.symlink",
                    "Data directory is a symlink — verify it points to a trusted location"
                        .to_string(),
                ));
                warnings += 1;
            }

            // 5. Check env vars for common misconfigs
            if std::env::var("WORKFORCE_GATEWAY_TOKEN").is_err()
                && std::env::var("GATEWAY_TOKEN").is_err()
            {
                findings.push((
                    "INFO",
                    "env.no_gateway_token",
                    "No WORKFORCE_GATEWAY_TOKEN env var set (ok if using config file)".to_string(),
                ));
                info_count += 1;
            }

            // Print findings
            for (severity, check_id, detail) in &findings {
                let icon = match *severity {
                    "CRITICAL" => style("✗ CRITICAL").red().bold(),
                    "WARN" => style("⚠ WARNING ").yellow(),
                    "OK" => style("✓ OK      ").green(),
                    _ => style("ℹ INFO    ").blue(),
                };
                println!("  {} [{}] {}", icon, check_id, detail);
            }

            // Summary
            println!();
            if critical > 0 {
                println!(
                    "  {} {} critical, {} warnings, {} info",
                    style("Result:").bold(),
                    style(critical).red().bold(),
                    style(warnings).yellow(),
                    info_count,
                );
                std::process::exit(1);
            } else if warnings > 0 {
                println!(
                    "  {} 0 critical, {} warnings, {} info",
                    style("Result:").bold(),
                    style(warnings).yellow(),
                    info_count,
                );
            } else {
                println!(
                    "  {} {} All checks passed ({} info)",
                    style("Result:").bold(),
                    style("✓").green(),
                    info_count,
                );
            }
        }
        Commands::Worker { command } => {
            use workforce_worker::{backup, doctor, refresh, serve, WorkerConfig};
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .try_init();
            match command {
                WorkerCommand::Serve { config } => {
                    serve::serve(WorkerConfig::load(&config)?).await?;
                }
                WorkerCommand::Refresh {
                    config,
                    golden,
                    changed,
                } => {
                    let worker = serve::worker(WorkerConfig::load(&config)?).await?;
                    let selection = match (golden.as_deref(), changed) {
                        (_, true) => refresh::Selection::Changed,
                        (Some(name), false) => refresh::Selection::Named(name),
                        (None, false) => refresh::Selection::All,
                    };
                    let report = refresh::refresh(&worker, selection, chrono::Utc::now()).await?;
                    println!("{}", refresh::describe(&report));
                    if !report.failed.is_empty() {
                        std::process::exit(1);
                    }
                }
                WorkerCommand::Backup { config } => {
                    let worker = serve::worker(WorkerConfig::load(&config)?).await?;
                    let report = backup::backup(&worker, chrono::Utc::now()).await?;
                    println!("{}", backup::describe(&report));
                }
                WorkerCommand::Doctor { config } => {
                    let config = WorkerConfig::load(&config)?;
                    let checks = doctor::doctor(
                        &config,
                        &workforce_worker::host::HostZfs,
                        &workforce_worker::host::HostContainers,
                        &*workforce_worker::host::cloud(&config),
                        &workforce_worker::host::HostRepos::default(),
                        chrono::Utc::now(),
                    )
                    .await;
                    println!("{}", doctor::describe(&checks));
                    if checks.iter().any(|check| !check.ok) {
                        std::process::exit(1);
                    }
                }
            }
        }
        Commands::LocalAgent { command } => match command {
            LocalAgentCommand::Connect {
                gateway,
                token,
                id,
                name,
                workdir,
                mcp,
                mcp_tools,
                mcp_token_file,
            } => {
                let agent_id = id.unwrap_or_else(local_agent::default_node_id);
                let agent_name = name.unwrap_or_else(|| agent_id.clone());
                let tunnel = match mcp {
                    Some(url) => {
                        let bearer = match std::env::var(local_agent::MCP_TOKEN_ENV) {
                            Ok(bearer) => bearer,
                            Err(_) => {
                                let path = mcp_token_file.ok_or_else(|| {
                                    eyre::eyre!(
                                        "--mcp needs its bearer in {} or --mcp-token-file",
                                        local_agent::MCP_TOKEN_ENV
                                    )
                                })?;
                                std::fs::read_to_string(&path)
                                    .map_err(|e| eyre::eyre!("reading {}: {e}", path.display()))?
                            }
                        };
                        Some(local_agent::McpTunnel::new(
                            &url,
                            bearer.trim().to_string(),
                            mcp_tools,
                        )?)
                    }
                    None => None,
                };
                local_agent::run_local_agent(
                    &gateway,
                    &token,
                    &agent_id,
                    &agent_name,
                    workdir,
                    tunnel,
                )
                .await?;
            }
        },
        Commands::Workflow {
            command,
            workflows_dir,
            data_dir,
        } => {
            use workforce_orchestrator::workflow::definition::WorkflowDefinition;

            let workflows_dir = cli.workdir.join(&workflows_dir);

            match command {
                WorkflowCommand::List { gateway, token } => {
                    let defs: Vec<WorkflowDefinition> =
                        if let (Some(gw), Some(tok)) = (&gateway, &token) {
                            let rest = workforce_gateway_client::GatewayRestClient::new(gw, tok);
                            let sources = rest.export_workflows().await?;
                            sources
                                .values()
                                .filter_map(|toml| WorkflowDefinition::from_toml(toml).ok())
                                .collect()
                        } else {
                            WorkflowDefinition::load_dir(&workflows_dir)?
                        };

                    let source_label = if gateway.is_some() {
                        format!("on {}", gateway.as_deref().unwrap_or("server"))
                    } else {
                        format!("in {}", workflows_dir.display())
                    };

                    if defs.is_empty() {
                        println!(
                            "{} No workflows found {}",
                            style("!").yellow(),
                            source_label
                        );
                    } else {
                        println!(
                            "{} {} workflows {}\n",
                            style("Workflows").bold(),
                            defs.len(),
                            style(&source_label).dim()
                        );
                        println!(
                            "  {:<20} {:<30} {:<8} {}",
                            style("ID").bold(),
                            style("Name").bold(),
                            style("Steps").bold(),
                            style("Description").bold(),
                        );
                        for def in &defs {
                            let desc: String = def.workflow.description.chars().take(50).collect();
                            println!(
                                "  {:<20} {:<30} {:<8} {}",
                                style(&def.workflow.id).cyan(),
                                def.workflow.name,
                                def.steps.len(),
                                style(desc).dim(),
                            );
                        }
                    }
                }
                WorkflowCommand::Validate { path } => match WorkflowDefinition::from_file(&path) {
                    Ok(def) => {
                        println!(
                            "{} Workflow '{}' is valid ({} steps)",
                            style("✓").green().bold(),
                            style(&def.workflow.id).cyan(),
                            def.steps.len()
                        );
                        for (i, step) in def.steps.iter().enumerate() {
                            let markers = [
                                if step.exit { Some("exit") } else { None },
                                step.pause.as_deref().map(|_| "pause"),
                                if !step.branches.is_empty() {
                                    Some("branches")
                                } else {
                                    None
                                },
                                step.goto.as_deref().map(|_| "goto"),
                                step.handoff.as_deref().map(|_| "handoff"),
                                step.skill.as_deref().map(|_| "skill"),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>();
                            let marker_str = if markers.is_empty() {
                                String::new()
                            } else {
                                format!(" [{}]", markers.join(", "))
                            };
                            println!(
                                "  {}. {} — {}{}",
                                i + 1,
                                style(&step.id).cyan(),
                                step.name,
                                style(marker_str).dim(),
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("{} Validation failed: {:#}", style("✗").red().bold(), e);
                        std::process::exit(1);
                    }
                },
                WorkflowCommand::Show { id, gateway, token } => {
                    let def_opt: Option<WorkflowDefinition> =
                        if let (Some(gw), Some(tok)) = (&gateway, &token) {
                            let rest = workforce_gateway_client::GatewayRestClient::new(gw, tok);
                            match rest.get_workflow_source(&id).await {
                                Ok(toml) => WorkflowDefinition::from_toml(&toml).ok(),
                                Err(_) => None,
                            }
                        } else {
                            let defs = WorkflowDefinition::load_dir(&workflows_dir)?;
                            defs.into_iter().find(|d| d.workflow.id == id)
                        };

                    let source_label = if gateway.is_some() {
                        format!("on {}", gateway.as_deref().unwrap_or("server"))
                    } else {
                        format!("in {}", workflows_dir.display())
                    };

                    match def_opt {
                        Some(ref def) => {
                            println!(
                                "{} Workflow: {} (v{})\n",
                                style("⚙").bold(),
                                style(&def.workflow.name).cyan().bold(),
                                def.workflow.version
                            );
                            if !def.workflow.description.is_empty() {
                                println!("  {}\n", def.workflow.description);
                            }
                            if let Some(agent) = &def.workflow.agent {
                                println!("  Default agent: {}", style(agent).yellow());
                            }
                            if !def.workflow.context.is_empty() {
                                println!("  Context inputs:");
                                for (key, field) in &def.workflow.context {
                                    let req = if field.required {
                                        style("required").red()
                                    } else {
                                        style("optional").dim()
                                    };
                                    println!(
                                        "    {} ({}) {}",
                                        style(key).cyan(),
                                        req,
                                        if field.description.is_empty() {
                                            String::new()
                                        } else {
                                            format!("— {}", field.description)
                                        }
                                    );
                                }
                            }
                            println!("\n  Steps ({}):", def.steps.len());
                            for (i, step) in def.steps.iter().enumerate() {
                                println!(
                                    "    {}. {} — {}",
                                    i + 1,
                                    style(&step.id).cyan(),
                                    step.name
                                );
                                if !step.branches.is_empty() {
                                    for b in &step.branches {
                                        println!(
                                            "       when {} → {}",
                                            style(&b.when).yellow(),
                                            style(&b.goto).cyan()
                                        );
                                    }
                                }
                                if let Some(goto) = &step.goto {
                                    println!("       goto → {}", style(goto).cyan());
                                }
                                if let Some(pause) = &step.pause {
                                    println!("       pause: {}", style(pause).yellow());
                                }
                                if step.exit {
                                    println!("       {}", style("exit").red());
                                }
                            }
                        }
                        None => {
                            eprintln!(
                                "{} Workflow '{}' not found {}",
                                style("✗").red().bold(),
                                id,
                                source_label
                            );
                            std::process::exit(1);
                        }
                    }
                }
                WorkflowCommand::Instances { status, workflow } => {
                    use workforce_common::persistence::SqliteStore;

                    std::fs::create_dir_all(&data_dir)?;
                    let db_path = data_dir.join("telemetry.db");
                    let store = SqliteStore::open(&db_path)?;

                    let instances =
                        store.list_workflow_instances(status.as_deref(), workflow.as_deref())?;

                    if instances.is_empty() {
                        println!("{} No workflow instances found.", style("!").yellow());
                    } else {
                        println!(
                            "{:<38} {:<20} {:<12} {:<20} {}",
                            style("ID").bold(),
                            style("Workflow").bold(),
                            style("Status").bold(),
                            style("Current Step").bold(),
                            style("Updated").bold(),
                        );
                        for inst in &instances {
                            let status_styled = match inst.status.as_str() {
                                "running" => style(&inst.status).green(),
                                "paused" => style(&inst.status).yellow(),
                                "completed" => style(&inst.status).cyan(),
                                "failed" => style(&inst.status).red(),
                                "exited" => style(&inst.status).magenta(),
                                _ => style(&inst.status).dim(),
                            };
                            let step = inst.current_step_id.as_deref().unwrap_or("-");
                            let updated = inst.updated_at.get(..19).unwrap_or(&inst.updated_at);
                            println!(
                                "{:<38} {:<20} {:<12} {:<20} {}",
                                &inst.id[..std::cmp::min(inst.id.len(), 36)],
                                inst.workflow_id,
                                status_styled,
                                step,
                                updated,
                            );
                        }
                        println!("\n  {} instances shown.", style(instances.len()).bold());
                    }
                }
                WorkflowCommand::Status { id } => {
                    use workforce_common::persistence::SqliteStore;

                    std::fs::create_dir_all(&data_dir)?;
                    let db_path = data_dir.join("telemetry.db");
                    let store = SqliteStore::open(&db_path)?;

                    match store.get_workflow_instance(&id)? {
                        Some(inst) => {
                            let status_styled = match inst.status.as_str() {
                                "running" => style(&inst.status).green().to_string(),
                                "paused" => style(&inst.status).yellow().to_string(),
                                "completed" => style(&inst.status).cyan().to_string(),
                                "failed" => style(&inst.status).red().to_string(),
                                "exited" => style(&inst.status).magenta().to_string(),
                                _ => inst.status.clone(),
                            };
                            println!(
                                "{} Workflow Instance {}\n",
                                style("⚙").bold(),
                                style(&inst.id).cyan()
                            );
                            println!("  Workflow:     {}", inst.workflow_id);
                            println!("  Status:       {}", status_styled);
                            println!(
                                "  Current step: {}",
                                inst.current_step_id.as_deref().unwrap_or("-")
                            );
                            if let Some(agent) = &inst.agent_id {
                                println!("  Agent:        {}", agent);
                            }
                            println!("  Created:      {}", inst.created_at);
                            println!("  Updated:      {}", inst.updated_at);

                            // Parse and display step history
                            if let Ok(history) =
                                serde_json::from_str::<Vec<serde_json::Value>>(&inst.step_history)
                            {
                                if !history.is_empty() {
                                    println!("\n  Step history ({}):", history.len());
                                    for (i, step) in history.iter().enumerate() {
                                        let step_id = step
                                            .get("step_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let failed = step
                                            .get("failed")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let outcome = if failed {
                                            "failed"
                                        } else {
                                            step.get("outcome")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("continued")
                                        };
                                        let rounds = step
                                            .get("rounds")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        let cost = step
                                            .get("cost_usd")
                                            .and_then(|v| v.as_f64())
                                            .unwrap_or(0.0);
                                        println!(
                                            "    {}. {} (outcome: {}, {} rounds, ${:.4})",
                                            i + 1,
                                            style(step_id).cyan(),
                                            style(outcome).yellow(),
                                            rounds,
                                            cost,
                                        );
                                    }
                                }
                            }
                        }
                        None => {
                            eprintln!("{} Instance '{}' not found.", style("✗").red().bold(), id);
                            std::process::exit(1);
                        }
                    }
                }
                WorkflowCommand::Create { id } => {
                    let path = workflows_dir.join(format!("{}.toml", id));
                    if path.exists() {
                        eprintln!(
                            "{} Workflow '{}' already exists at {}",
                            style("✗").red().bold(),
                            id,
                            path.display()
                        );
                        std::process::exit(1);
                    }
                    std::fs::create_dir_all(&workflows_dir)?;
                    let scaffold = format!(
                        r#"[workflow]
id = "{id}"
name = ""
description = ""

[[step]]
id = "start"
name = ""
task = """
Describe what the agent should do in this step.
OUTCOME: done
"""
exit = true
"#
                    );
                    std::fs::write(&path, scaffold)?;
                    println!(
                        "{} Created workflow scaffold at {}",
                        style("✓").green().bold(),
                        style(path.display()).cyan()
                    );
                }
                WorkflowCommand::Pull {
                    ids,
                    gateway,
                    token,
                } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);
                    std::fs::create_dir_all(&workflows_dir)?;

                    if ids.is_empty() {
                        // Pull all workflows
                        let workflows = rest.export_workflows().await?;
                        if workflows.is_empty() {
                            println!("{} No workflows found on server.", style("!").yellow());
                        } else {
                            for (id, toml) in &workflows {
                                let path = workflows_dir.join(format!("{}.toml", id));
                                std::fs::write(&path, toml)?;
                                println!(
                                    "  {} {}",
                                    style("↓").green(),
                                    style(path.display()).cyan()
                                );
                            }
                            println!(
                                "\n{} Pulled {} workflow(s).",
                                style("✓").green().bold(),
                                workflows.len()
                            );
                        }
                    } else {
                        // Pull specific workflows
                        for id in &ids {
                            match rest.get_workflow_source(id).await {
                                Ok(toml) => {
                                    let path = workflows_dir.join(format!("{}.toml", id));
                                    std::fs::write(&path, &toml)?;
                                    println!(
                                        "  {} {}",
                                        style("↓").green(),
                                        style(path.display()).cyan()
                                    );
                                }
                                Err(e) => {
                                    eprintln!("  {} {}: {}", style("✗").red(), id, e);
                                }
                            }
                        }
                    }
                }
                WorkflowCommand::Push {
                    paths,
                    gateway,
                    token,
                } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);

                    let files: Vec<PathBuf> = if paths.is_empty() {
                        // Push all TOML files in workflows dir
                        if workflows_dir.exists() {
                            std::fs::read_dir(&workflows_dir)?
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        paths
                    };

                    if files.is_empty() {
                        println!("{} No workflow files to push.", style("!").yellow());
                    } else {
                        let mut pushed = 0;
                        for path in &files {
                            let content = std::fs::read_to_string(path)?;
                            // Validate locally first
                            match WorkflowDefinition::from_toml(&content).and_then(|def| match def
                                .authoring_issues()
                            {
                                issues if issues.is_empty() => Ok(def),
                                issues => Err(eyre::eyre!(
                                    "will not run as written:\n      - {}",
                                    issues.join("\n      - ")
                                )),
                            }) {
                                Ok(def) => {
                                    match rest.put_workflow(&def.workflow.id, &content).await {
                                        Ok(()) => {
                                            println!(
                                                "  {} {} ({})",
                                                style("↑").green(),
                                                style(&def.workflow.id).cyan(),
                                                style(path.display()).dim()
                                            );
                                            pushed += 1;
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "  {} {}: {}",
                                                style("✗").red(),
                                                path.display(),
                                                e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("  {} {}: {}", style("✗").red(), path.display(), e);
                                }
                            }
                        }
                        println!(
                            "\n{} Pushed {} workflow(s).",
                            style("✓").green().bold(),
                            pushed
                        );
                    }
                }
                WorkflowCommand::Delete { id, gateway, token } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);

                    match rest.delete_workflow(&id).await {
                        Ok(()) => {
                            println!(
                                "{} Deleted workflow '{}' from server.",
                                style("✓").green().bold(),
                                style(&id).cyan()
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "{} Failed to delete '{}': {}",
                                style("✗").red().bold(),
                                id,
                                e
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Playbook {
            command,
            playbooks_dir,
        } => {
            use workforce_orchestrator::playbook::PlaybookDefinition;

            let playbooks_dir = cli.workdir.join(&playbooks_dir);

            match command {
                PlaybookCommand::List { gateway, token } => {
                    let defs: Vec<PlaybookDefinition> =
                        if let (Some(gw), Some(tok)) = (&gateway, &token) {
                            let rest = workforce_gateway_client::GatewayRestClient::new(gw, tok);
                            let sources = rest.export_playbooks().await?;
                            sources
                                .values()
                                .filter_map(|toml| PlaybookDefinition::from_toml(toml).ok())
                                .collect()
                        } else {
                            PlaybookDefinition::load_dir(&playbooks_dir)?
                        };
                    let source_label = if gateway.is_some() {
                        format!("on {}", gateway.as_deref().unwrap_or("server"))
                    } else {
                        format!("in {}", playbooks_dir.display())
                    };
                    if defs.is_empty() {
                        println!(
                            "{} No playbooks found {}",
                            style("!").yellow(),
                            source_label
                        );
                    } else {
                        println!(
                            "{} {} playbooks {}\n",
                            style("Playbooks").bold(),
                            defs.len(),
                            style(&source_label).dim()
                        );
                        for def in &defs {
                            let desc: String = def.playbook.description.chars().take(60).collect();
                            println!(
                                "  {:<24} {:<28} {} steps  {}",
                                style(&def.playbook.id).cyan(),
                                def.playbook.name,
                                def.steps.len(),
                                style(desc).dim(),
                            );
                        }
                    }
                }
                PlaybookCommand::Validate { path } => match PlaybookDefinition::from_file(&path) {
                    Ok(def) => {
                        println!(
                            "{} Playbook '{}' is valid ({} steps)",
                            style("✓").green().bold(),
                            style(&def.playbook.id).cyan(),
                            def.steps.len()
                        );
                        for (i, step) in def.steps.iter().enumerate() {
                            let artifact = step
                                .artifact
                                .as_deref()
                                .map(|a| format!(" [artifact: {a}]"))
                                .unwrap_or_default();
                            println!(
                                "  {}. {} — {}{}",
                                i + 1,
                                style(&step.id).cyan(),
                                step.title,
                                style(artifact).dim(),
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("{} Validation failed: {:#}", style("✗").red().bold(), e);
                        std::process::exit(1);
                    }
                },
                PlaybookCommand::Show {
                    id,
                    gateway,
                    token,
                    owner,
                } => {
                    let def_opt: Option<PlaybookDefinition> =
                        if let (Some(gw), Some(tok)) = (&gateway, &token) {
                            let rest = workforce_gateway_client::GatewayRestClient::new(gw, tok);
                            match rest.get_playbook_source(&id, owner.as_deref()).await {
                                Ok(toml) => PlaybookDefinition::from_toml(&toml).ok(),
                                Err(_) => None,
                            }
                        } else {
                            PlaybookDefinition::load_dir(&playbooks_dir)?
                                .into_iter()
                                .find(|d| d.playbook.id == id)
                        };
                    match def_opt {
                        Some(def) => {
                            println!(
                                "{} Playbook: {}\n",
                                style("▶").bold(),
                                style(&def.playbook.name).cyan().bold(),
                            );
                            if !def.playbook.description.is_empty() {
                                println!("  {}\n", def.playbook.description);
                            }
                            for (i, step) in def.steps.iter().enumerate() {
                                println!(
                                    "  {}. {} ({})",
                                    i + 1,
                                    style(&step.title).bold(),
                                    style(&step.id).dim(),
                                );
                                if let Some(a) = &step.artifact {
                                    println!("     artifact: {}", style(a).yellow());
                                }
                            }
                        }
                        None => {
                            let where_ = if let Some(gw) = &gateway {
                                format!("on {gw}")
                            } else {
                                format!("in {}", playbooks_dir.display())
                            };
                            eprintln!(
                                "{} Playbook '{}' not found {}",
                                style("✗").red().bold(),
                                id,
                                where_
                            );
                            std::process::exit(1);
                        }
                    }
                }
                PlaybookCommand::Create { id } => {
                    let path = playbooks_dir.join(format!("{id}.toml"));
                    if path.exists() {
                        eprintln!(
                            "{} Playbook '{}' already exists at {}",
                            style("✗").red().bold(),
                            id,
                            path.display()
                        );
                        std::process::exit(1);
                    }
                    std::fs::create_dir_all(&playbooks_dir)?;
                    let scaffold = format!(
                        r#"[playbook]
id = "{id}"
name = ""
description = ""

[[step]]
id = "step-1"
title = ""
# artifact = "output.md"   # optional: a file this step writes for later steps
prompt = """
Describe what you should do in this step and what to discuss with the user.
"""
"#
                    );
                    std::fs::write(&path, scaffold)?;
                    println!(
                        "{} Created playbook scaffold at {}",
                        style("✓").green().bold(),
                        style(path.display()).cyan()
                    );
                }
                PlaybookCommand::Pull {
                    ids,
                    gateway,
                    token,
                    owner,
                } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);
                    std::fs::create_dir_all(&playbooks_dir)?;
                    if ids.is_empty() {
                        let playbooks = rest.export_playbooks().await?;
                        if playbooks.is_empty() {
                            println!("{} No playbooks found on server.", style("!").yellow());
                        } else {
                            for (id, toml) in &playbooks {
                                let path = playbooks_dir.join(format!("{id}.toml"));
                                std::fs::write(&path, toml)?;
                                println!(
                                    "  {} {}",
                                    style("↓").green(),
                                    style(path.display()).cyan()
                                );
                            }
                            println!(
                                "\n{} Pulled {} playbook(s).",
                                style("✓").green().bold(),
                                playbooks.len()
                            );
                        }
                    } else {
                        for id in &ids {
                            match rest.get_playbook_source(id, owner.as_deref()).await {
                                Ok(toml) => {
                                    let path = playbooks_dir.join(format!("{id}.toml"));
                                    std::fs::write(&path, &toml)?;
                                    println!(
                                        "  {} {}",
                                        style("↓").green(),
                                        style(path.display()).cyan()
                                    );
                                }
                                Err(e) => eprintln!("  {} {}: {}", style("✗").red(), id, e),
                            }
                        }
                    }
                }
                PlaybookCommand::Push {
                    paths,
                    gateway,
                    token,
                } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);
                    let files: Vec<PathBuf> = if paths.is_empty() {
                        if playbooks_dir.exists() {
                            std::fs::read_dir(&playbooks_dir)?
                                .filter_map(|e| e.ok())
                                .map(|e| e.path())
                                .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
                                .collect()
                        } else {
                            Vec::new()
                        }
                    } else {
                        paths
                    };
                    if files.is_empty() {
                        println!("{} No playbook files to push.", style("!").yellow());
                    } else {
                        let mut pushed = 0;
                        for path in &files {
                            let content = std::fs::read_to_string(path)?;
                            match PlaybookDefinition::from_toml(&content) {
                                Ok(def) => {
                                    match rest.put_playbook(&def.playbook.id, &content).await {
                                        Ok(()) => {
                                            println!(
                                                "  {} {} ({})",
                                                style("↑").green(),
                                                style(&def.playbook.id).cyan(),
                                                style(path.display()).dim()
                                            );
                                            pushed += 1;
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "  {} {}: {}",
                                                style("✗").red(),
                                                path.display(),
                                                e
                                            )
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!("  {} {}: {}", style("✗").red(), path.display(), e)
                                }
                            }
                        }
                        println!(
                            "\n{} Pushed {} playbook(s).",
                            style("✓").green().bold(),
                            pushed
                        );
                    }
                }
                PlaybookCommand::Delete { id, gateway, token } => {
                    let rest = workforce_gateway_client::GatewayRestClient::new(&gateway, &token);
                    match rest.delete_playbook(&id).await {
                        Ok(()) => println!(
                            "{} Deleted playbook '{}' from server.",
                            style("✓").green().bold(),
                            style(&id).cyan()
                        ),
                        Err(e) => {
                            eprintln!(
                                "{} Failed to delete '{}': {}",
                                style("✗").red().bold(),
                                id,
                                e
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Activity {
            agent,
            last,
            from,
            to,
            verbose,
            gateway: _,
            token: _,
        } => {
            use chrono::Utc;
            use console::style;
            use workforce_common::activity_log::{
                parse_absolute_time, parse_relative_time, ActivityQuery,
            };
            use workforce_common::telemetry::TelemetryStore;

            // For now, use the local telemetry database - in production this would use gateway
            let db_path = cli.workdir.join("data/telemetry.db");

            if !db_path.exists() {
                eprintln!(
                    "Error: Telemetry database not found at {}",
                    db_path.display()
                );
                eprintln!(
                    "Make sure the workforce service is running or has been run at least once."
                );
                std::process::exit(1);
            }

            // Parse time range
            let (from_time, to_time) = if let Some(last_str) = &last {
                let from = parse_relative_time(last_str)?;
                let to = Utc::now();
                (Some(from), Some(to))
            } else if let (Some(from_str), Some(to_str)) = (&from, &to) {
                let from = parse_absolute_time(from_str)?;
                let to = parse_absolute_time(to_str)?;
                (Some(from), Some(to))
            } else {
                // Default to last 1 hour
                let to = Utc::now();
                let from = to - chrono::Duration::hours(1);
                (Some(from), Some(to))
            };

            let store: std::sync::Arc<dyn TelemetryStore> =
                std::sync::Arc::new(workforce_common::persistence::SqliteStore::open(&db_path)?);
            let query = ActivityQuery {
                agent_id: agent.clone(),
                start_time: from_time,
                end_time: to_time,
                verbose,
            };

            let activities = store.query_activities(&query)?;

            // Display header
            println!("\n{}", style("Agent Activity Report").bold());
            println!("{}", style("====================").bold());
            println!(
                "Time Range: {} - {} UTC",
                from_time.map_or("start".to_string(), |t| t
                    .format("%Y-%m-%d %H:%M")
                    .to_string()),
                to_time.map_or("now".to_string(), |t| t
                    .format("%Y-%m-%d %H:%M")
                    .to_string())
            );
            if let Some(agent_id) = &agent {
                println!("Agent: {}\n", agent_id);
            } else {
                println!("Agent: All\n");
            }

            if activities.is_empty() {
                println!("No activity found in the specified time range.");
            } else {
                // Display activities
                use workforce_common::activity_log::ActivityType;

                for activity in &activities {
                    let time_str = activity.timestamp.format("%H:%M:%S");

                    match activity.activity_type {
                        ActivityType::SessionStarted | ActivityType::SessionCompleted => {
                            println!("{} | {}", style(time_str).dim(), activity.description);
                        }
                        ActivityType::ToolExecution => {
                            let status = if activity.success.unwrap_or(true) {
                                "✓"
                            } else {
                                "✗"
                            };
                            let duration_str = activity
                                .duration_ms
                                .map_or("?".to_string(), |ms| format!("{}ms", ms));
                            println!(
                                "{} | {} {} {}",
                                style(time_str).dim(),
                                activity.description,
                                if activity.success.unwrap_or(true) {
                                    style(status).green()
                                } else {
                                    style(status).red()
                                },
                                style(duration_str).dim()
                            );

                            if verbose {
                                if let Some(metadata) = &activity.metadata {
                                    println!(
                                        "      Metadata: {}",
                                        serde_json::to_string_pretty(metadata)?
                                    );
                                }
                            }
                        }
                        ActivityType::LlmCall => {
                            println!("{} | {}", style(time_str).dim(), activity.description);

                            if verbose {
                                if let Some(metadata) = &activity.metadata {
                                    println!(
                                        "      Metadata: {}",
                                        serde_json::to_string_pretty(metadata)?
                                    );
                                }
                            }
                        }
                    }
                }

                // Summary statistics
                println!("\n{}", style("Summary").bold());
                println!("{}", style("-------").bold());

                let tool_calls = activities
                    .iter()
                    .filter(|a| matches!(a.activity_type, ActivityType::ToolExecution))
                    .count();
                let llm_calls = activities
                    .iter()
                    .filter(|a| matches!(a.activity_type, ActivityType::LlmCall))
                    .count();
                let sessions = activities
                    .iter()
                    .filter(|a| matches!(a.activity_type, ActivityType::SessionStarted))
                    .count();

                println!("Sessions: {}", sessions);
                println!("LLM Calls: {}", llm_calls);
                println!("Tool Calls: {}", tool_calls);
            }
        }

        Commands::Migrate {
            config,
            identity_db,
            dry_run,
        } => {
            use workforce_common::settings::WorkforceConfig;
            use workforce_identity::sqlite::SqliteIdentityStore;
            use workforce_identity::store::IdentityStore;
            use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

            println!("Loading config from: {}", config.display());
            let config_content = std::fs::read_to_string(&config)?;
            let wf_config: WorkforceConfig = toml::from_str(&config_content)?;

            if dry_run {
                println!("[DRY RUN] No changes will be made.\n");
            }

            // Step 1: Open/create identity store
            println!("=== Step 1: Identity Store ===");
            if !dry_run {
                if let Some(parent) = identity_db.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let store = SqliteIdentityStore::new(&identity_db)?;
            let existing_count = store.count()?;
            println!("  Existing identities: {}", existing_count);

            // Step 2: Bootstrap admin if empty
            println!("\n=== Step 2: Bootstrap Admin ===");
            if existing_count == 0 {
                if dry_run {
                    println!("  [DRY RUN] Would create admin from ADMIN_EMAIL/ADMIN_PASSWORD");
                } else {
                    workforce_hub::jwt::bootstrap_admin(&store)?;
                    if store.count()? > 0 {
                        println!("  Admin created from environment variables");
                    } else {
                        println!("  No ADMIN_EMAIL/ADMIN_PASSWORD set — skipped");
                    }
                }
            } else {
                println!("  Identities already exist — skipping bootstrap");
            }

            // Step 3: Seed identities from sentinel config (owner_slack_ids)
            println!("\n=== Step 3: Seed Identities from Sentinel Config ===");
            for slack_id in &wf_config.sentinel.owner_slack_ids {
                let existing = store.get_by_linked_id("slack", slack_id)?;
                if let Some(ref id) = existing {
                    println!("  Slack {} → already linked to {}", slack_id, id.id);
                } else if dry_run {
                    println!("  [DRY RUN] Would create identity for Slack {}", slack_id);
                } else {
                    let identity = Identity::new(slack_id.as_str(), IdentityRole::SystemAdmin)
                        .with_status(IdentityStatus::Active)
                        .with_linked_id("slack", slack_id.as_str());
                    store.create(&identity)?;
                    println!(
                        "  Created identity {} for Slack {} (SystemAdmin)",
                        identity.id, slack_id
                    );
                }
            }

            // Step 4: Seed identities from user_map entries
            println!("\n=== Step 4: Seed Identities from User Maps ===");
            for agent in &wf_config.agents {
                for (slack_id, friendly_name) in &agent.user_map {
                    let existing = store.get_by_linked_id("slack", slack_id)?;
                    if existing.is_some() {
                        continue;
                    }
                    if dry_run {
                        println!(
                            "  [DRY RUN] Would create identity for {} ({})",
                            friendly_name, slack_id
                        );
                    } else {
                        let identity = Identity::new(friendly_name.as_str(), IdentityRole::Member)
                            .with_linked_id("slack", slack_id.as_str());
                        store.create(&identity)?;
                        println!(
                            "  Created identity {} for {} ({})",
                            identity.id, friendly_name, slack_id
                        );
                    }
                }
            }

            // Step 5: Assign agent ownership
            println!("\n=== Step 5: Agent Ownership ===");
            let admin = store
                .list(&workforce_identity::store::IdentityFilter {
                    role: Some(IdentityRole::SystemAdmin),
                    ..Default::default()
                })?
                .into_iter()
                .next();

            if let Some(ref admin) = admin {
                for agent in &wf_config.agents {
                    if agent.owner_id.is_some() {
                        println!("  Agent '{}' already has owner", agent.id);
                    } else if dry_run {
                        println!(
                            "  [DRY RUN] Would assign agent '{}' to admin {}",
                            agent.id, admin.id
                        );
                    } else {
                        println!(
                            "  Agent '{}' → assigned to admin {} (update config manually)",
                            agent.id, admin.id
                        );
                    }
                }
            } else {
                println!("  No admin identity found — skipping ownership assignment");
            }

            // Step 6: Summary
            let final_count = store.count()?;
            println!("\n=== Summary ===");
            println!("  Total identities: {}", final_count);
            if dry_run {
                println!("\n  Re-run without --dry-run to apply changes.");
            } else {
                println!(
                    "\n  Migration complete. Identity database: {}",
                    identity_db.display()
                );
                println!("  To assign agent ownership, add owner_id to each [[agents]] section in workforce.toml.");
            }
        }

        Commands::MigrateTelemetry { config, from } => {
            use eyre::WrapErr;
            use workforce_common::settings::WorkforceConfig;
            use workforce_common::telemetry_migrate::migrate_sqlite_to_postgres;

            println!("Loading config from: {}", config.display());
            // Use the same loader as `serve` so `${VAR}` env expansion runs
            // (settings::from_toml) — otherwise a `[postgres] password =
            // "${WORKFORCE_PG_PASSWORD}"` is sent to Postgres verbatim and
            // auth fails. A raw `toml::from_str` skips that expansion.
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` with your connection details before running this command.",
                        config.display()
                    );
                }
            };

            let sqlite_path = from.unwrap_or_else(|| {
                std::path::PathBuf::from(&wf_config.hub.data_dir).join("telemetry.db")
            });

            if !sqlite_path.exists() {
                eyre::bail!(
                    "source SQLite telemetry database not found at {}",
                    sqlite_path.display()
                );
            }

            println!("Source SQLite: {}", sqlite_path.display());
            println!(
                "Destination Postgres: {}",
                pg_cfg.url.as_deref().unwrap_or("<from host/port/database>")
            );
            println!("Running migrations and copying rows...");

            let report = migrate_sqlite_to_postgres(&sqlite_path, pg_cfg)
                .await
                .context("migrate-telemetry failed")?;

            println!("\nMigration complete — rows copied:");
            println!("  tasks:                    {}", report.tasks);
            println!("  audit_log:                {}", report.audit_log);
            println!("  cost_records:             {}", report.cost_records);
            println!("  task_telemetry:           {}", report.task_telemetry);
            println!("  round_telemetry:          {}", report.round_telemetry);
            println!("  tool_telemetry:           {}", report.tool_telemetry);
            println!("  model_health_events:      {}", report.model_health_events);
            println!(
                "  agent_model_preferences:  {}",
                report.agent_model_preferences
            );
            println!("  policy_proposals:         {}", report.policy_proposals);
            println!("  model_pricing:            {}", report.model_pricing);
            println!("  workflow_instances:       {}", report.workflow_instances);
            println!("  security_events:          {}", report.security_events);
            println!("  ─────────────────────");
            println!("  total:                    {}", report.total());
            println!(
                "\nNext step: set `[telemetry] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }

        Commands::MigrateSessions { config } => {
            use eyre::WrapErr;
            use workforce_common::session_migrate::migrate_sessions_to_postgres;
            use workforce_common::settings::WorkforceConfig;

            println!("Loading config from: {}", config.display());
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` with your connection details before running this command.",
                        config.display()
                    );
                }
            };

            let workspaces = migrate_agent_workspaces(&wf_config, &config)?;

            println!(
                "Destination Postgres: {}",
                pg_cfg.url.as_deref().unwrap_or("<from host/port/database>")
            );
            for (agent, ws) in &workspaces {
                println!("  agent {agent}: {}", ws.display());
            }
            println!("Running migrations and copying sessions...");

            let report = migrate_sessions_to_postgres(&workspaces, pg_cfg)
                .await
                .context("migrate-sessions failed")?;

            println!("\nMigration complete — rows copied:");
            println!("  sessions:          {}", report.sessions);
            println!("  archive_messages:  {}", report.archive_messages);
            if report.force_sealed > 0 {
                println!(
                    "  force_sealed:      {} (duplicate active sessions — newest kept active)",
                    report.force_sealed
                );
            }
            if report.truncated > 0 {
                println!(
                    "  truncated:         {} (oversized live blobs cut to newest messages; \
                     archive keeps full history — ids in the log)",
                    report.truncated
                );
            }
            if !report.failed.is_empty() {
                println!("  FAILED:            {}", report.failed.len());
                for id in &report.failed {
                    println!("    {id}");
                }
                eyre::bail!(
                    "{} session(s) failed to copy — fix the sources above and re-run \
                     (idempotent; already-copied sessions are skipped)",
                    report.failed.len()
                );
            }
            println!(
                "\nNext step: set `[sessions] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }

        Commands::MigrateScheduler { config } => {
            use eyre::WrapErr;
            use workforce_common::settings::WorkforceConfig;
            use workforce_hub::scheduler_store::postgres as scheduler_pg;

            println!("Loading config from: {}", config.display());
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` with your connection details before running this command.",
                        config.display()
                    );
                }
            };

            let workspaces = migrate_agent_workspaces(&wf_config, &config)?;

            let pool = scheduler_pg::build_pool(pg_cfg).context("building scheduler pool")?;
            scheduler_pg::run_migrations(&pool).await?;

            let mut total_copied = 0usize;
            let mut total_skipped = 0usize;
            for (agent_id, workspace) in &workspaces {
                let jobs_file = workforce_hub::scheduler_store::jobs_file_path(workspace);
                let (copied, skipped) =
                    scheduler_pg::migrate_jobs_file(&pool, agent_id, &jobs_file)
                        .await
                        .with_context(|| format!("migrating jobs for agent {agent_id}"))?;
                println!("  agent {agent_id}: {copied} copied, {skipped} already present");
                total_copied += copied;
                total_skipped += skipped;
            }
            println!(
                "\nMigration complete — {total_copied} job(s) copied, {total_skipped} already present."
            );
            println!(
                "\nNext step: set `[scheduler_store] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }

        Commands::MigrateIdentity { config, from } => {
            use eyre::WrapErr;
            use workforce_common::settings::WorkforceConfig;
            use workforce_identity::postgres_migrate::migrate_sqlite_to_postgres;

            println!("Loading config from: {}", config.display());
            // Use the same loader as `serve` so `${VAR}` env expansion runs
            // (settings::from_toml) — otherwise a `[postgres] password =
            // "${WORKFORCE_PG_PASSWORD}"` is sent to Postgres verbatim and
            // auth fails. A raw `toml::from_str` skips that expansion.
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` first.",
                        config.display()
                    );
                }
            };

            let sqlite_path = from.unwrap_or_else(|| {
                std::path::PathBuf::from(&wf_config.hub.data_dir).join("identities.db")
            });
            if !sqlite_path.exists() {
                eyre::bail!(
                    "source SQLite identity database not found at {}",
                    sqlite_path.display()
                );
            }

            println!("Source SQLite: {}", sqlite_path.display());
            println!("Running migrations and copying rows...");
            let report = migrate_sqlite_to_postgres(&sqlite_path, pg_cfg)
                .await
                .context("migrate-identity failed")?;

            println!("\nMigration complete — rows copied:");
            println!("  identities: {}", report.identities);
            println!("  teams:      {}", report.teams);
            println!("  total:      {}", report.total());
            println!(
                "\nNext step: set `[identity] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }

        Commands::MigratePairing { config, from } => {
            use eyre::WrapErr;
            use workforce_common::settings::WorkforceConfig;
            use workforce_pairing::postgres_migrate::migrate_sqlite_to_postgres;

            println!("Loading config from: {}", config.display());
            // Use the same loader as `serve` so `${VAR}` env expansion runs
            // (settings::from_toml) — otherwise a `[postgres] password =
            // "${WORKFORCE_PG_PASSWORD}"` is sent to Postgres verbatim and
            // auth fails. A raw `toml::from_str` skips that expansion.
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` first.",
                        config.display()
                    );
                }
            };

            let sqlite_path = from.unwrap_or_else(|| {
                std::path::PathBuf::from(&wf_config.hub.data_dir).join("pairing.db")
            });
            if !sqlite_path.exists() {
                eyre::bail!(
                    "source SQLite pairing database not found at {}",
                    sqlite_path.display()
                );
            }

            println!("Source SQLite: {}", sqlite_path.display());
            println!("Running migrations and copying rows...");
            let report = migrate_sqlite_to_postgres(&sqlite_path, pg_cfg)
                .await
                .context("migrate-pairing failed")?;

            println!("\nMigration complete — rows copied:");
            println!(
                "  channel_pairing_requests: {}",
                report.channel_pairing_requests
            );
            println!("  channel_allowlist:        {}", report.channel_allowlist);
            println!(
                "  device_pairing_requests:  {}",
                report.device_pairing_requests
            );
            println!("  approved_devices:         {}", report.approved_devices);
            println!(
                "  node_pairing_requests:    {}",
                report.node_pairing_requests
            );
            println!("  ─────────────────────");
            println!("  total:                    {}", report.total());
            println!(
                "\nNext step: set `[pairing_store] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }

        Commands::MigrateMcpStore { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| PathBuf::from(&c.hub.data_dir).join("mcp_store.db"),
                "mcp_store",
                |p, cfg| async move {
                    Ok(
                        workforce_mcp_store::postgres_migrate::migrate_sqlite_to_postgres(&p, &cfg)
                            .await?
                            .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigrateProjects { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| PathBuf::from(&c.hub.data_dir).join("projects.db"),
                "projects",
                |p, cfg| async move {
                    Ok(
                        workforce_projects::postgres_migrate::migrate_sqlite_to_postgres(&p, &cfg)
                            .await?
                            .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigratePermissions { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| PathBuf::from(&c.hub.data_dir).join("permissions.db"),
                "permissions",
                |p, cfg| async move {
                    Ok(
                        workforce_permissions::postgres_migrate::migrate_sqlite_to_postgres(
                            &p, &cfg,
                        )
                        .await?
                        .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigrateTraining { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| resolve_relative_db(&c.hub.data_dir, &c.training.db_path),
                "training",
                |p, cfg| async move {
                    Ok(
                        workforce_training::postgres_migrate::migrate_sqlite_to_postgres(&p, &cfg)
                            .await?
                            .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigrateSelfLearning { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| resolve_relative_db(&c.hub.data_dir, &c.self_learning.db_path),
                "self_learning",
                |p, cfg| async move {
                    Ok(
                        workforce_self_learning::postgres_migrate::migrate_sqlite_to_postgres(
                            &p, &cfg,
                        )
                        .await?
                        .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigrateApprovals { config, from } => {
            run_store_backfill(
                &config,
                from,
                |c| PathBuf::from(&c.hub.data_dir).join("approvals.db"),
                "approvals",
                |p, cfg| async move {
                    Ok(
                        workforce_hub::approvals::store::postgres_migrate::migrate_sqlite_to_postgres(
                            &p, &cfg,
                        )
                        .await?
                        .total(),
                    )
                },
            )
            .await?;
        }

        Commands::MigrateCodegraph { config, from } => {
            use eyre::WrapErr;
            use workforce_codebase_knowledge::graph::postgres_migrate::migrate_sqlite_to_postgres;
            use workforce_common::settings::WorkforceConfig;

            println!("Loading config from: {}", config.display());
            // Use the same loader as `serve` so `${VAR}` env expansion runs
            // (settings::from_toml) — otherwise a `[postgres] password =
            // "${WORKFORCE_PG_PASSWORD}"` is sent to Postgres verbatim and
            // auth fails. A raw `toml::from_str` skips that expansion.
            let wf_config = WorkforceConfig::load(&config)?;

            let pg_cfg = match wf_config.postgres.as_ref().filter(|p| p.enabled) {
                Some(cfg) => cfg,
                None => {
                    eyre::bail!(
                        "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` first.",
                        config.display()
                    );
                }
            };

            let sqlite_path = from.unwrap_or_else(|| {
                std::path::PathBuf::from(&wf_config.hub.data_dir)
                    .join(".workforce")
                    .join("knowledge")
                    .join("codegraph.db")
            });
            if !sqlite_path.exists() {
                eyre::bail!(
                    "source SQLite code graph database not found at {}",
                    sqlite_path.display()
                );
            }

            println!("Source SQLite: {}", sqlite_path.display());
            println!("Running migrations and copying rows...");
            let report = migrate_sqlite_to_postgres(&sqlite_path, pg_cfg)
                .await
                .context("migrate-codegraph failed")?;

            println!("\nMigration complete — rows copied:");
            println!("  repositories:  {}", report.repositories);
            println!("  entities:      {}", report.entities);
            println!("  relationships: {}", report.relationships);
            println!("  summaries:     {}", report.summaries);
            println!("  file_index:    {}", report.file_index);
            println!("  index_state:   {}", report.index_state);
            if report.vectors_carried {
                println!(
                    "  code vectors:  the graph's identity, {} fill(s) and {} pending file(s) carried across: what a vector database keeps for this graph is kept for it still, and nothing is embedded again",
                    report.vector_fills, report.vector_pending
                );
            } else {
                println!(
                    "  code vectors:  not carried across: the destination was a code graph of its own already, or the source predates graph identities. The repos copied in are filled from the graph"
                );
            }
            println!("  ─────────────────");
            println!("  total:         {}", report.total());
            println!(
                "\nNext step: set `[codegraph] backend = \"postgres\"` in {} and restart the hub.",
                config.display()
            );
        }
        Commands::Memory { command } => {
            handle_memory_command(command).await?;
        }
        Commands::Shadow { command } => {
            shadow::handle(command).await?;
        }
        Commands::Artifacts {
            command:
                ArtifactsCommand::Reindex {
                    config,
                    mirror,
                    embed,
                },
        } => {
            let wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
            let outside = embed.seeding()?;
            if let Some(seeding) = &outside {
                println!(
                    "  comparing {} with the node over a sample of the artifacts before anything is written: the node embeds it with its own model, which may take a minute",
                    seeding.embedder.name()
                );
            }
            let done = workforce_hub::serve::reindex_artifacts(
                &wf_config,
                mirror,
                outside,
                &print_equivalence,
                |indexed, total| println!("  {indexed} of {total} artifacts"),
            )
            .await?;
            if done.failed > 0 {
                for failure in &done.failures {
                    eprintln!("  {} not indexed: {}", failure.id, failure.error);
                }
                eyre::bail!(
                    "{} of {} artifacts could not be indexed; the index is not complete, and artifact search goes by keyword until it is",
                    done.failed,
                    done.total
                );
            }
            println!(
                "{} {} artifacts indexed, {} stale vectors removed; artifact search relies on the index",
                style("✓").green().bold(),
                done.indexed,
                done.removed
            );
        }
        Commands::Code {
            command:
                CodeCommand::ReindexVectors {
                    config,
                    repo,
                    data_dir,
                    rate,
                    force,
                    take_over,
                    repos_at_once,
                    embed,
                },
        } => {
            let mut wf_config = workforce_common::settings::WorkforceConfig::load(&config)?;
            if let Some(data_dir) = data_dir {
                wf_config.knowledge.data_dir = Some(data_dir.to_string_lossy().to_string());
            }
            let outside = embed.seeding()?;
            let per_sec = rate.unwrap_or(match outside {
                Some(_) => 0,
                None => wf_config.knowledge.vector_backfill_per_sec,
            });
            let began = std::time::Instant::now();
            let last = std::sync::Mutex::new((std::time::Instant::now(), 0u64));
            let seeded = workforce_hub::seeding::reindex_code_vectors(
                &wf_config,
                repo.as_deref(),
                workforce_hub::seeding::SeedOptions {
                    repos_at_once: repos_at_once.unwrap_or(match outside {
                        Some(_) => 4,
                        None => 1,
                    }),
                    outside,
                    per_sec,
                    sample: 200,
                    force,
                    take_over,
                    note: |said| println!("  {said}"),
                },
                &print_equivalence,
                &|repo, fill| {
                    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                    if last.0.elapsed() < std::time::Duration::from_secs(10)
                        && fill.stage
                            != workforce_codebase_knowledge::graph::vectors::FillStage::Done
                    {
                        return;
                    }
                    let rate = fill.done.saturating_sub(last.1) as f64
                        / last.0.elapsed().as_secs_f64().max(0.001);
                    *last = (std::time::Instant::now(), fill.done);
                    println!(
                        "  {repo}: {} of {} stored ({}), {rate:.0}/s",
                        fill.done,
                        fill.total,
                        fill.stage.as_str()
                    );
                },
            )
            .await?;
            let mut short = 0;
            for repo in &seeded {
                let c = &repo.confirmed;
                println!(
                    "  {} ({}): {} of {} entities, {} of {} summaries",
                    repo.name,
                    repo.repo_id,
                    c.entities_stored,
                    c.entities_expected,
                    c.summaries_stored,
                    c.summaries_expected
                );
                if !c.agrees() {
                    short += 1;
                }
            }
            if short > 0 {
                eyre::bail!(
                    "vellev does not hold what the code graph says it should for {short} repo(s), listed above. If they were being indexed meanwhile, run this again once they are not: it compares each with the graph and puts right what differs. If it still fails, run it with --force (and --repo <id> for one repo), which writes every vector again and removes what the graph has not; the repo's vectors go on being searched meanwhile"
                );
            }
            println!(
                "{} the code vectors of {} repo(s) are complete in vellev ({:.0}s)",
                style("✓").green().bold(),
                seeded.len(),
                began.elapsed().as_secs_f64()
            );
        }
        Commands::Permissions { command } => {
            handle_permissions_command(command).await?;
        }
        Commands::Docs { command } => {
            docs::run(command).await?;
        }
        Commands::Training { command } => {
            training::run(command).await?;
        }
        Commands::Decide { command } => {
            decide::run(command).await?;
        }
        Commands::Eval { command } => {
            evals::run(command).await?;
        }
        Commands::Export {
            agents,
            out,
            config,
            slack_team_id,
        } => {
            handle_export_command(agents, out, config, slack_team_id).await?;
        }
        Commands::Import {
            bundle,
            config,
            plan_only,
            rename,
            map_identity,
            yes,
            slack_team_id,
        } => {
            handle_import_command(
                bundle,
                config,
                plan_only,
                rename,
                map_identity,
                yes,
                slack_team_id,
            )
            .await?;
        }
    }

    Ok(())
}

/// Weaviate readiness budget for one-shot CLI commands. Unlike the hub boot
/// (which rides out a cold cluster restart for minutes), an interactive
/// command runs against an already-live system, so it fails fast.
const CLI_MEMORY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Build the memory store for one-shot CLI commands. Honors `[memory] backend`
/// and fails closed — never a silent empty in-memory store on a SQLite install.
async fn build_memory_store(
    wf_config: &workforce_common::settings::WorkforceConfig,
) -> Result<Arc<dyn workforce_memory::store::MemoryStore>> {
    workforce_hub::state::resolve_memory_store_with_budget(wf_config, CLI_MEMORY_CONNECT_TIMEOUT)
        .await
}

fn install_id() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "workforce".to_string())
}

async fn handle_export_command(
    agents: Vec<String>,
    out: PathBuf,
    config: PathBuf,
    slack_team_id: Option<String>,
) -> Result<()> {
    use workforce_common::settings::WorkforceConfig;

    let wf_config = WorkforceConfig::load(&config)?;
    let identity = build_identity_store(&wf_config).await?.as_identity_store();
    let memory = build_memory_store(&wf_config).await?;
    let locks = workforce_common::locking::LockManager::new();
    let ctx = workforce_hub::transfer::TransferContext {
        data_dir: PathBuf::from(&wf_config.hub.data_dir),
        base_config_path: config.clone(),
        config: &wf_config,
        identity_store: identity.as_ref(),
        memory_store: memory.as_ref(),
        lock_manager: &locks,
        def_store: None,
        actor: None,
        install_id: install_id(),
        slack_team_id,
    };

    let file = std::fs::File::create(&out)?;
    let (_file, counts) = workforce_hub::transfer::export_bundle(&ctx, &agents, file).await?;
    println!(
        "{} exported {} agent(s), {} session(s), {} memorie(s), {} identitie(s) → {}",
        style("✓").green().bold(),
        counts.agents,
        counts.sessions,
        counts.memories,
        counts.identities,
        style(out.display()).dim(),
    );
    Ok(())
}

async fn handle_import_command(
    bundle: PathBuf,
    config: PathBuf,
    plan_only: bool,
    rename: Vec<(String, String)>,
    map_identity: Vec<(String, String)>,
    yes: bool,
    slack_team_id: Option<String>,
) -> Result<()> {
    use std::io::Cursor;
    use workforce_common::settings::WorkforceConfig;
    use workforce_hub::transfer::{apply_import, plan_import, ImportOptions, TransferContext};

    let wf_config = WorkforceConfig::load(&config)?;
    let identity = build_identity_store(&wf_config).await?.as_identity_store();
    let memory = build_memory_store(&wf_config).await?;
    let locks = workforce_common::locking::LockManager::new();
    let bytes = std::fs::read(&bundle)?;
    let mut options = ImportOptions {
        renames: rename.into_iter().collect(),
        identity_overrides: map_identity.into_iter().collect(),
        confirm: false,
    };
    let ctx = TransferContext {
        data_dir: PathBuf::from(&wf_config.hub.data_dir),
        base_config_path: config.clone(),
        config: &wf_config,
        identity_store: identity.as_ref(),
        memory_store: memory.as_ref(),
        lock_manager: &locks,
        def_store: None,
        actor: None,
        install_id: install_id(),
        slack_team_id,
    };

    let plan = plan_import(&ctx, Cursor::new(bytes.clone()), &options)?;
    print_merge_plan(&plan);
    if plan_only {
        return Ok(());
    }
    if !yes && !confirm_prompt()? {
        println!("Aborted.");
        return Ok(());
    }
    options.confirm = true;
    let outcome = apply_import(&ctx, Cursor::new(bytes), &options).await?;
    print_import_outcome(&outcome);
    Ok(())
}

fn print_merge_plan(plan: &workforce_hub::transfer::MergePlan) {
    println!(
        "\n{} merge plan (source install: {})",
        style("→").bold(),
        style(&plan.source_install_id).dim()
    );
    for agent in &plan.agents {
        let rename = if agent.source_id == agent.target_id {
            String::new()
        } else {
            format!(" → {}", style(&agent.target_id).yellow())
        };
        let collision = if agent.collides { " (collision)" } else { "" };
        println!(
            "  agent {}{}{}  · {} session(s), {} memorie(s)",
            style(&agent.source_id).bold(),
            rename,
            style(collision).red(),
            agent.sessions,
            agent.memories,
        );
        for credential in &agent.expected_credentials {
            println!("      provision: {}", style(credential).dim());
        }
    }
    let created = plan.identities.iter().filter(|i| i.is_create()).count();
    println!(
        "  identities: {} matched, {} to create",
        plan.identities.len() - created,
        created
    );
}

fn print_import_outcome(outcome: &workforce_hub::transfer::ImportOutcome) {
    println!(
        "{} imported {} agent(s) · {} session(s) · {} memorie(s) ({} skipped) · {} identitie(s) created, {} matched",
        style("✓").green().bold(),
        outcome.agents_imported.len(),
        outcome.sessions_imported,
        outcome.memories_imported,
        outcome.memories_skipped,
        outcome.identities_created,
        outcome.identities_matched,
    );
    for (from, to) in &outcome.renames_applied {
        println!("  renamed {from} → {}", style(to).yellow());
    }
    for note in &outcome.notes {
        println!("  {} {note}", style("!").yellow());
    }
}

fn confirm_prompt() -> Result<bool> {
    use std::io::Write;
    print!("Apply this import? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("y"))
}

async fn handle_permissions_command(command: PermissionsCommand) -> Result<()> {
    use eyre::WrapErr;
    use workforce_common::settings::WorkforceConfig;
    use workforce_policy::rules::PolicyConfig;

    match command {
        PermissionsCommand::Migrate {
            config,
            apply,
            csv_out,
        } => {
            let wf_config = WorkforceConfig::load(&config)?;
            let policy_path =
                wf_config.hub.policy_path.clone().ok_or_else(|| {
                    eyre::eyre!("config has no `[hub] policy_path` to migrate from")
                })?;
            println!(
                "{} reading policy YAML at {}...",
                style("📜").bold(),
                style(&policy_path).dim()
            );
            let policy_config = PolicyConfig::from_file(&policy_path)
                .with_context(|| format!("loading policy YAML at {policy_path}"))?;

            let report =
                workforce_permissions::migration::translate_rules(&policy_config.rules, None);
            println!(
                "{} {} rule(s) would migrate · {} skipped",
                style("→").bold(),
                style(report.migrated_count()).green(),
                style(report.skipped_count()).yellow()
            );
            for migrated in &report.migrated {
                println!(
                    "   {} {:>5}  {:<10}  {}",
                    style("•").dim(),
                    style(migrated.state.as_str()).bold(),
                    migrated.scope.as_str(),
                    migrated.tool_pattern,
                );
            }
            for skipped in &report.skipped {
                println!(
                    "   {} skip  {}  ({})  pattern={}",
                    style("·").dim(),
                    skipped.name,
                    skipped.reason.as_str(),
                    skipped.tool_pattern,
                );
            }

            // Role-scope CSV — only written when there's content.
            if !report.role_csv.is_empty() {
                let csv_path = csv_out.unwrap_or_else(|| {
                    PathBuf::from(&wf_config.hub.data_dir).join("permissions-role-migration.csv")
                });
                std::fs::write(&csv_path, report.role_csv_text())
                    .with_context(|| format!("writing role CSV at {}", csv_path.display()))?;
                println!(
                    "{} role-scope CSV → {}",
                    style("📄").bold(),
                    style(csv_path.display()).dim()
                );
            }

            if !apply {
                println!(
                    "{} dry-run only — pass --apply to write {} row(s) into the permission store",
                    style("ℹ").cyan(),
                    report.migrated_count()
                );
                return Ok(());
            }

            let Some(store) = build_permission_store(&wf_config).await? else {
                eyre::bail!(
                    "[permissions] enabled = false in config; set it to true to apply the migration"
                );
            };
            let written = workforce_permissions::migration::apply_report(&report, &*store)?;
            println!(
                "{} wrote {} rule(s) into the permission store",
                style("✓").green().bold(),
                written
            );
        }
        PermissionsCommand::ImportRoles {
            config,
            from_csv,
            apply,
        } => {
            let wf_config = WorkforceConfig::load(&config)?;
            println!(
                "{} reading edited role CSV at {}...",
                style("📥").bold(),
                style(from_csv.display()).dim()
            );
            let parsed = parse_role_import_csv(&from_csv)?;
            println!(
                "{} {} row(s) would import · {} skipped (missing team_id)",
                style("→").bold(),
                style(parsed.importable.len()).green(),
                style(parsed.skipped.len()).yellow()
            );
            for r in &parsed.importable {
                println!(
                    "   {} {:>5}  team={}  pattern={}",
                    style("•").dim(),
                    style(r.state.as_str()).bold(),
                    style(&r.scope_id).dim(),
                    r.tool_pattern,
                );
            }
            for s in &parsed.skipped {
                println!(
                    "   {} skip  {}  pattern={}",
                    style("·").dim(),
                    s.name,
                    s.tool_pattern,
                );
            }
            if !apply {
                println!(
                    "{} dry-run only — pass --apply to write {} row(s) into the permission store",
                    style("ℹ").cyan(),
                    parsed.importable.len()
                );
                return Ok(());
            }
            let Some(store) = build_permission_store(&wf_config).await? else {
                eyre::bail!(
                    "[permissions] enabled = false in config; set it to true to apply the import"
                );
            };
            let mut written = 0usize;
            for rule in &parsed.importable {
                store.upsert_rule(rule)?;
                written += 1;
            }
            println!(
                "{} wrote {} rule(s) into the permission store",
                style("✓").green().bold(),
                written
            );
        }
        PermissionsCommand::Matrix { command } => handle_matrix_command(command).await?,
    }
    Ok(())
}

/// Break-glass for the admin-editable capability matrix. Operates directly on
/// the override store; a running hub's reloader converges the change within ~5s.
/// Precisely *because* it takes effect live, a CLI write bypasses the gateway's
/// `permissions_manage` gate — so the mutating arms (`set`/`reset`) are guarded
/// by `WORKFORCE_ALLOW_LOCAL_MATRIX_WRITE`, keeping an admin's agent shell from
/// flipping authorization state out of band. `show` is read-only and ungated.
async fn handle_matrix_command(command: MatrixCommand) -> Result<()> {
    use eyre::WrapErr;
    use workforce_common::settings::WorkforceConfig;
    use workforce_hub::authz::{
        baseline_for, decision_to_stored, is_protected_admin_cap, Capability, Decision,
    };
    use workforce_permissions::CapabilityChange;

    match command {
        MatrixCommand::Show { config } => {
            let wf_config = WorkforceConfig::load(&config)?;
            let store = build_capability_store(&wf_config).await?;
            let overrides = store
                .list_overrides()
                .wrap_err("listing capability overrides")?;
            if overrides.is_empty() {
                println!(
                    "{} no overrides — every cell is at its built-in default",
                    style("ℹ").cyan()
                );
            }
            for o in &overrides {
                println!(
                    "   {} {:<13} {:<22} {}",
                    style("•").dim(),
                    o.role,
                    o.capability,
                    style(o.decision.as_str()).bold(),
                );
            }
        }
        MatrixCommand::Reset { config } => {
            guard_local_matrix_mutation("reset");
            let wf_config = WorkforceConfig::load(&config)?;
            let store = build_capability_store(&wf_config).await?;
            let changes = store
                .list_overrides()
                .wrap_err("listing capability overrides")?
                .into_iter()
                .map(|o| CapabilityChange {
                    role: o.role,
                    capability: o.capability,
                    decision: None,
                })
                .collect::<Vec<_>>();
            let cleared = changes.len();
            store
                .apply_changes(&changes, None)
                .wrap_err("clearing capability overrides")?;
            println!(
                "{} cleared {cleared} override(s); a running hub converges within ~5s",
                style("✓").green().bold(),
            );
        }
        MatrixCommand::Set {
            config,
            role,
            capability,
            decision,
        } => {
            guard_local_matrix_mutation("set");
            if role.trim().is_empty() {
                eyre::bail!("role must be a non-empty role key");
            }
            let cap = Capability::from_key(&capability)
                .ok_or_else(|| eyre::eyre!("unknown capability '{capability}'"))?;
            if is_protected_admin_cap(cap) {
                eyre::bail!(
                    "capability '{capability}' is reserved for system_admin and cannot be overridden"
                );
            }
            let parsed = match decision.as_str() {
                "allow" => Decision::Allow,
                "ask" => Decision::Ask,
                "deny" => Decision::Deny,
                other => eyre::bail!("decision must be 'allow', 'ask', or 'deny' (got '{other}')"),
            };
            let wf_config = WorkforceConfig::load(&config)?;
            let store = build_capability_store(&wf_config).await?;
            let known_role = matches!(role.as_str(), "member" | "manager" | "system_admin")
                || store.list_custom_roles()?.iter().any(|r| r.key == role);
            if !known_role {
                eyre::bail!("unknown role '{role}' (not a built-in or a defined custom role)");
            }
            let stored = (parsed != baseline_for(&role, cap)).then(|| decision_to_stored(parsed));
            store
                .apply_changes(
                    &[CapabilityChange {
                        role,
                        capability,
                        decision: stored,
                    }],
                    None,
                )
                .wrap_err("applying capability override")?;
            println!(
                "{} override applied; a running hub converges within ~5s",
                style("✓").green().bold(),
            );
        }
    }
    Ok(())
}

/// Output of [`parse_role_import_csv`]. Same shape as the migrate
/// dry-run report: "rules that would land" plus "rules skipped, with
/// enough context to inspect."
#[derive(Debug)]
struct RoleImportPlan {
    importable: Vec<workforce_permissions::types::PermissionRule>,
    skipped: Vec<RoleImportSkip>,
}

#[derive(Debug)]
struct RoleImportSkip {
    name: String,
    tool_pattern: String,
}

/// Parse the operator-edited role-scope CSV. Expected columns
/// (header row required): `name,role,team_id,tool_pattern,action,
/// priority`. The `name` and `role` columns are read for context only.
fn parse_role_import_csv(path: &Path) -> Result<RoleImportPlan> {
    use uuid::Uuid;
    use workforce_permissions::types::{
        PermissionOrigin, PermissionRule, PermissionScope, PermissionState,
    };

    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_path(path)?;
    let mut importable = Vec::new();
    let mut skipped = Vec::new();
    for (row_idx, row) in reader.deserialize::<RoleImportRow>().enumerate() {
        let row = row.map_err(|e| eyre::eyre!("row {}: {e}", row_idx + 2))?;
        if row.team_id.trim().is_empty() {
            skipped.push(RoleImportSkip {
                name: row.name,
                tool_pattern: row.tool_pattern,
            });
            continue;
        }
        Uuid::parse_str(row.team_id.trim()).map_err(|_| {
            eyre::eyre!(
                "row {}: team_id '{}' is not a UUID",
                row_idx + 2,
                row.team_id
            )
        })?;
        let state = match row.action.trim() {
            "allow" => PermissionState::Allow,
            "ask" | "require_approval" => PermissionState::Ask,
            "always_ask" => PermissionState::AlwaysAsk,
            "deny" => PermissionState::Deny,
            other => {
                eyre::bail!(
                    "row {}: unknown action '{}' (expected allow / ask / always_ask / deny)",
                    row_idx + 2,
                    other
                );
            }
        };
        importable.push(PermissionRule::new(
            PermissionScope::Team,
            row.team_id.trim(),
            row.tool_pattern.trim(),
            state,
            PermissionOrigin::Admin,
        ));
    }
    Ok(RoleImportPlan {
        importable,
        skipped,
    })
}

#[derive(serde::Deserialize)]
struct RoleImportRow {
    name: String,
    #[serde(default, rename = "role")]
    _role: String,
    team_id: String,
    tool_pattern: String,
    action: String,
    #[serde(default, rename = "priority")]
    _priority: Option<i32>,
}

/// The URL of the store memories are kept in, to tell the operator.
fn memory_store_url(config: &workforce_common::settings::WorkforceConfig) -> &str {
    if config.memory.is_vellev() {
        &config.vellev.url
    } else {
        &config.weaviate.url
    }
}

/// What `serve` refuses before it starts anything: the artifact index
/// served from vellev while a shadow of it is kept or not promoted
/// (DW-676). Memories are guarded where they are opened.
async fn guard_before_serving(config: &workforce_common::settings::WorkforceConfig) -> Result<()> {
    workforce_hub::promotion::guard_artifacts(config).await
}

async fn handle_memory_command(command: MemoryCommand) -> Result<()> {
    use workforce_common::settings::WorkforceConfig;

    match command {
        MemoryCommand::MoveEdge {
            step,
            apply,
            agents,
            edge_url,
            edge_tenant,
            edge_model,
            edge_dimensions,
            edge_query_prefix,
            known_customers,
            config,
        } => {
            use workforce_hub::edge_memory_move as moving;
            let wf_config = WorkforceConfig::load(&config)?;
            let api_key = std::env::var("EDGE_VELLEV_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .ok_or_else(|| eyre::eyre!("set EDGE_VELLEV_API_KEY to the edge tenant's key"))?;
            let edge = moving::EdgeTenant {
                url: edge_url,
                api_key,
                tenant: edge_tenant,
                model: edge_model,
                dimensions: edge_dimensions,
                query_prefix: edge_query_prefix,
            };
            let stores = moving::open(&wf_config, &edge, &agents).await?;
            let survey = match step {
                MoveEdgeStep::Copy => {
                    let path = known_customers.ok_or_else(|| {
                        eyre::eyre!("copy needs --known-customers: the customers the edge still knows, so that no erased customer's memory is copied")
                    })?;
                    let known: std::collections::BTreeSet<String> = std::fs::read_to_string(&path)?
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(str::to_string)
                        .collect();
                    let copied = moving::copy(&stores, &agents, &known, apply).await?;
                    println!(
                        "  {} customer memories of customers the edge no longer knows (purged, never copied)",
                        copied.survey.erased.len()
                    );
                    println!(
                        "copy: {} edge objects of WorkforceMemory to copy, {} already in the edge's tenant, {} sent",
                        copied.survey.to_copy.len(),
                        copied.already,
                        copied.sent
                    );
                    copied.survey
                }
                MoveEdgeStep::Purge => {
                    let purged = moving::purge(&stores, &agents, apply).await?;
                    println!(
                        "purge: {} edge memories to purge, {} purged",
                        purged.survey.to_purge.len(),
                        purged.purged
                    );
                    purged.survey
                }
            };
            for which in moving::HubStore::ALL {
                println!(
                    "  {}: {} edge objects",
                    which.label(),
                    survey.held.get(&which).copied().unwrap_or(0)
                );
            }
            println!(
                "  {} memories only in the hub's copies (erased on the edge; purged, never copied)",
                survey.survivors.len()
            );
            println!(
                "  {} objects of the edge's agents under other identities (left alone)",
                survey.unclassified
            );
            if !apply {
                println!("dry run: nothing was changed; add --apply");
            }
            Ok(())
        }
        MemoryCommand::MoveSqlite {
            from,
            apply,
            json,
            config,
        } => {
            let wf_config = WorkforceConfig::load(&config)?;
            let counts = workforce_hub::sqlite_memory_move::run(&wf_config, &from, apply).await?;
            if json {
                println!("{}", serde_json::to_string(&counts)?);
            } else {
                println!(
                    "move-sqlite: {} memories in SQLite, {} with nothing to move, {} moved, {} read back as they were",
                    counts.in_sqlite, counts.unmovable, counts.moved, counts.verified
                );
            }
            if !apply {
                if !json {
                    println!("dry run: nothing was changed; add --apply");
                }
                return Ok(());
            }
            if !counts.complete() {
                eyre::bail!("the move is not complete: {counts:?}");
            }
            Ok(())
        }
        MemoryCommand::Doctor { config } => {
            let wf_config = WorkforceConfig::load(&config)?;
            let url = memory_store_url(&wf_config);
            println!(
                "{} Checking memory schema at {}...",
                style("🩺").bold(),
                style(url).dim()
            );
            let store = workforce_hub::embeddings::switched_memory_store(
                &wf_config,
                workforce_hub::embeddings::base_store(&wf_config)?,
            )
            .await?;
            match store.doctor().await {
                Ok(()) => {
                    println!("{} schema OK", style("✓").green().bold());
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{} {}", style("✗").red().bold(), e);
                    std::process::exit(1);
                }
            }
        }
        MemoryCommand::RebuildSchema {
            yes,
            backup,
            config,
        } => {
            let wf_config = WorkforceConfig::load(&config)?;
            let url = memory_store_url(&wf_config);
            let store = workforce_hub::embeddings::switched_memory_store(
                &wf_config,
                workforce_hub::embeddings::base_store(&wf_config)?,
            )
            .await?;

            println!(
                "{} Rebuilding memory schema at {}",
                style("🔧").bold(),
                style(url).dim()
            );

            println!("  1/4 Exporting existing objects…");
            let objects = store.export(true).await?;
            println!(
                "      exported {} object{}",
                style(objects.len()).cyan(),
                if objects.len() == 1 { "" } else { "s" }
            );

            let backup_path = backup.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "memory-backup-{}.json",
                    chrono::Utc::now().format("%Y%m%d-%H%M%S")
                ))
            });
            let backup_body = serde_json::json!({ "objects": objects });
            std::fs::write(&backup_path, serde_json::to_string_pretty(&backup_body)?)?;
            println!("      backup: {}", style(backup_path.display()).dim());

            if !yes {
                println!(
                    "\n{} dry-run — pass {} to actually rebuild.",
                    style("DRY RUN").yellow().bold(),
                    style("--yes").cyan()
                );
                println!("Would next:");
                println!("  2/4 DELETE class {}", store.collection());
                println!("  3/4 ensure_collection (creates fresh schema with pinned config)");
                println!("  4/4 POST {} objects back", objects.len());
                return Ok(());
            }

            println!("  2/4 Deleting class…");
            store.delete_collection().await?;

            println!("  3/4 Creating fresh schema…");
            store.ensure_collection().await?;

            println!("  4/4 Re-ingesting {} objects…", objects.len());
            let ingested = store.import(&objects).await?;

            println!(
                "\n{} rebuild complete: {}/{} objects re-ingested",
                style("✓").green().bold(),
                style(ingested).cyan(),
                objects.len()
            );
            if ingested != objects.len() {
                eprintln!(
                    "{} {} object(s) failed to re-ingest — check hub/weaviate logs for details",
                    style("!").yellow().bold(),
                    objects.len() - ingested
                );
            }
            println!("Backup preserved at {}", backup_path.display());
            Ok(())
        }
        MemoryCommand::Clean {
            apply,
            manifest,
            config,
        } => {
            use std::collections::BTreeMap;
            use workforce_memory::audit;

            let wf_config = WorkforceConfig::load(&config)?;
            let url = memory_store_url(&wf_config);
            let store = workforce_hub::embeddings::switched_memory_store(
                &wf_config,
                workforce_hub::embeddings::base_store(&wf_config)?,
            )
            .await?;

            println!(
                "{} Auditing memories at {}...",
                style("🧹").bold(),
                style(url).dim()
            );

            // Export raw objects so the classifier sees the same shape it
            // expects. We don't need vectors — pass `false`.
            let objects = store.export(false).await?;
            println!("  scanned {} object(s)", style(objects.len()).cyan());

            let (findings, summary) = audit::classify_with(
                &objects,
                &audit::DerivedFigureDetector::new(&wf_config.memory.derived_figures),
            );

            println!(
                "  recap_artifact:     {}",
                style(summary.recap_artifact_count).yellow()
            );
            println!(
                "  consolidation_meta: {}",
                style(summary.consolidation_meta_count).yellow()
            );
            println!(
                "  derived_figure:     {}",
                style(summary.derived_figure_count).yellow()
            );

            if findings.is_empty() {
                println!(
                    "\n{} no pollution detected — nothing to do.",
                    style("✓").green().bold()
                );
                return Ok(());
            }

            // Group findings by Weaviate UUID so a single object that
            // matches multiple classes (e.g. a consolidation-meta entry
            // that's also part of a recap cluster) gets a single PATCH
            // carrying both kind tags.
            let mut by_uuid: BTreeMap<String, (audit::Finding, Vec<audit::Classification>)> =
                BTreeMap::new();
            for f in &findings {
                by_uuid
                    .entry(f.weaviate_id.clone())
                    .and_modify(|(_, v)| v.push(f.classification))
                    .or_insert_with(|| (f.clone(), vec![f.classification]));
            }

            // Build object index for the apply phase.
            let mut props_by_uuid: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
            for obj in &objects {
                if let Some(id) = obj["id"].as_str() {
                    props_by_uuid.insert(id.to_string(), obj);
                }
            }

            // Show a sample.
            println!("\nSample (first 5):");
            for f in findings.iter().take(5) {
                println!(
                    "  [{}] {} {}",
                    style(f.classification.as_kind_tag()).cyan(),
                    style(&f.memory_id).dim(),
                    style(f.created_at.format("%Y-%m-%d %H:%M:%S")).dim()
                );
                println!("    {}", f.content_preview.lines().next().unwrap_or(""));
            }

            // Manifest path defaults to a timestamped file in CWD.
            let manifest_path = manifest.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "memory-clean-{}.json",
                    chrono::Utc::now().format("%Y%m%d-%H%M%S")
                ))
            });
            let manifest_body = serde_json::json!({
                "summary": summary,
                "findings": findings,
            });
            std::fs::write(
                &manifest_path,
                serde_json::to_string_pretty(&manifest_body)?,
            )?;
            println!(
                "\nManifest written to {}",
                style(manifest_path.display()).dim()
            );

            if !apply {
                println!(
                    "\n{} dry-run — pass {} to apply tag changes.",
                    style("DRY RUN").yellow().bold(),
                    style("--apply").cyan()
                );
                return Ok(());
            }

            println!("\n{} applying tag changes...", style("→").bold());
            let mut patched = 0usize;
            let mut failed = 0usize;
            for (uuid, (finding, classes)) in by_uuid {
                let Some(props) = props_by_uuid.get(&uuid) else {
                    failed += 1;
                    continue;
                };
                let existing: Vec<String> = props["properties"]["tags"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let mut new_tags = existing;
                for class in classes {
                    if let Some(updated) = audit::tags_with_kind(&new_tags, class) {
                        new_tags = updated;
                    }
                }
                match store.patch_tags(&uuid, &new_tags).await {
                    Ok(()) => {
                        patched += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "{} failed to tag {}: {}",
                            style("✗").red(),
                            finding.memory_id,
                            e
                        );
                        failed += 1;
                    }
                }
            }

            println!(
                "\n{} tagged {} object(s){}",
                style("✓").green().bold(),
                style(patched).cyan(),
                if failed > 0 {
                    format!(", {} failed", style(failed).red())
                } else {
                    String::new()
                }
            );
            Ok(())
        }
        MemoryCommand::Backfill {
            since,
            agent,
            config,
        } => {
            let when: chrono::DateTime<chrono::Utc> = since
                .parse()
                .map_err(|e| eyre::eyre!("invalid --since timestamp '{since}': {e}"))?;
            let wf_config = WorkforceConfig::load(&config)?;
            let workspace = PathBuf::from(&wf_config.hub.data_dir)
                .join("agents")
                .join(&agent);
            if !workspace.exists() {
                eyre::bail!(
                    "agent workspace not found: {} — check --agent and [hub] data_dir",
                    workspace.display()
                );
            }
            workforce_hub::memory_consolidation::preseed_consolidation_cursor(&workspace, when)?;
            println!(
                "{} Cursor pre-seeded for agent {} at {}",
                style("✓").green().bold(),
                style(&agent).cyan(),
                style(when.to_rfc3339()).dim(),
            );
            println!(
                "  Next scheduled consolidation tick will re-scan every session with updated_at ≥ {}",
                style(when.to_rfc3339()).dim()
            );
            Ok(())
        }
        MemoryCommand::PassiveExtract {
            since,
            agent,
            dry_run,
            model,
            config,
        } => {
            use workforce_common::settings::WorkforceConfig;

            let when: chrono::DateTime<chrono::Utc> = since
                .parse()
                .map_err(|e| eyre::eyre!("invalid --since timestamp '{since}': {e}"))?;
            let wf_config = WorkforceConfig::load(&config)
                .map_err(|e| eyre::eyre!("Loading {}: {}", config.display(), e))?;
            let data_dir = PathBuf::from(&wf_config.hub.data_dir);

            let (provider_base, default_tier_key) =
                workforce_agent::llm::tiers::parse_provider_tier_key(&wf_config.llm.provider)
                    .map_err(|e| {
                        eyre::eyre!(
                            "Failed to parse provider '{}': {}",
                            wf_config.llm.provider,
                            e
                        )
                    })?;
            let (provider_kind, api_key) = resolve_provider_and_key(&provider_base, None)?;
            let thinking_budget = wf_config.llm.thinking_budget_tokens;
            let zai_sub = wf_config.llm.zai_subscription;
            let base_llm =
                create_provider_async(provider_kind, &api_key, None, thinking_budget, zai_sub)
                    .await?;
            let llm: Arc<dyn LlmProvider> =
                Arc::from(wrap_with_failover(base_llm, Some(&wf_config)));

            let resolved_model = resolve_extract_model(
                model.as_deref(),
                &wf_config.llm.default_model,
                &default_tier_key,
            );

            let store: Arc<dyn workforce_memory::store::MemoryStore> = if dry_run {
                Arc::new(workforce_memory::in_memory::InMemoryStore::new())
            } else {
                workforce_hub::state::resolve_memory_store_with_budget(
                    &wf_config,
                    CLI_MEMORY_CONNECT_TIMEOUT,
                )
                .await?
            };

            let agent_ids: Vec<AgentId> = agent.iter().map(AgentId::new).collect();
            let scope_label = if agent_ids.is_empty() {
                "all agents".to_string()
            } else {
                agent_ids
                    .iter()
                    .map(|a| a.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            eprintln!(
                "{} Passive-memory extract: scope={} since={} dry_run={}",
                style("🧠").bold(),
                style(scope_label).cyan(),
                style(when.to_rfc3339()).dim(),
                dry_run,
            );

            let opts = workforce_hub::passive_memory_backfill::BackfillOptions {
                agent_ids,
                since: Some(when),
                dry_run,
                model: resolved_model,
            };
            let report =
                workforce_hub::passive_memory_backfill::run(&data_dir, llm, store, opts).await?;
            println!(
                "{}",
                workforce_hub::passive_memory_backfill::format_report(&report)
            );
            workforce_hub::passive_memory_backfill::log_report_summary(&report);
            Ok(())
        }
        MemoryCommand::RelationshipScan {
            since,
            agent,
            dry_run,
            config,
        } => {
            use workforce_common::settings::WorkforceConfig;

            let when: chrono::DateTime<chrono::Utc> = since
                .parse()
                .map_err(|e| eyre::eyre!("invalid --since timestamp '{since}': {e}"))?;
            let wf_config = WorkforceConfig::load(&config)
                .map_err(|e| eyre::eyre!("Loading {}: {}", config.display(), e))?;
            let data_dir = PathBuf::from(&wf_config.hub.data_dir);

            let telemetry: Arc<dyn workforce_common::telemetry::TelemetryStore> = if dry_run {
                Arc::new(workforce_common::persistence::SqliteStore::in_memory()?)
            } else {
                workforce_hub::state::create_telemetry_store(&wf_config, true).await?
            };

            let agent_ids: Vec<AgentId> = agent.iter().map(AgentId::new).collect();
            let scope_label = if agent_ids.is_empty() {
                "all agents".to_string()
            } else {
                agent_ids
                    .iter()
                    .map(|a| a.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            eprintln!(
                "{} Relationship scan: scope={} since={} dry_run={}",
                style("🤝").bold(),
                style(scope_label).cyan(),
                style(when.to_rfc3339()).dim(),
                dry_run,
            );

            let opts = workforce_hub::relationship::backfill::RelationshipScanOptions {
                agent_ids,
                since: Some(when),
                dry_run,
            };
            let report = workforce_hub::relationship::backfill::run(&data_dir, telemetry, opts)?;
            println!(
                "{}",
                workforce_hub::relationship::backfill::format_report(&report)
            );
            workforce_hub::relationship::backfill::log_report_summary(&report);
            Ok(())
        }
    }
}

/// Resolve a configured store DB path: absolute as-is, otherwise joined
/// against `[hub].data_dir`. Mirrors each store's own path resolution.
fn resolve_relative_db(data_dir: &str, db_path: &str) -> PathBuf {
    let raw = std::path::Path::new(db_path);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        PathBuf::from(data_dir).join(raw)
    }
}

/// Resolve each agent's workspace exactly as the hub does — the agent's
/// configured `workspace`, else `{data_dir}/agents/{id}` — for the
/// per-workspace backfills (`migrate-sessions`, `migrate-scheduler`).
fn migrate_agent_workspaces(
    wf_config: &workforce_common::settings::WorkforceConfig,
    config_path: &std::path::Path,
) -> eyre::Result<Vec<(String, PathBuf)>> {
    let workspaces: Vec<(String, PathBuf)> = wf_config
        .agents
        .iter()
        .map(|a| {
            let ws = a.workspace.clone().map(PathBuf::from).unwrap_or_else(|| {
                PathBuf::from(&wf_config.hub.data_dir)
                    .join("agents")
                    .join(&a.id)
            });
            (a.id.clone(), ws)
        })
        .collect();
    if workspaces.is_empty() {
        eyre::bail!("no [[agents]] defined in {}", config_path.display());
    }
    Ok(workspaces)
}

/// Shared driver for the per-store SQLite→Postgres backfill subcommands:
/// load config (with `${VAR}` expansion), require `[postgres] enabled`,
/// resolve the source DB path (or `--from`), run the store's idempotent
/// migration, and print the count + next step. `label` is the TOML section
/// name (e.g. `mcp_store`).
async fn run_store_backfill<P, F, Fut>(
    config: &std::path::Path,
    from: Option<PathBuf>,
    default_path: P,
    label: &str,
    migrate: F,
) -> eyre::Result<()>
where
    P: FnOnce(&workforce_common::settings::WorkforceConfig) -> PathBuf,
    F: FnOnce(PathBuf, workforce_postgres::PostgresConfig) -> Fut,
    Fut: std::future::Future<Output = eyre::Result<usize>>,
{
    use eyre::WrapErr;
    use workforce_common::settings::WorkforceConfig;

    println!("Loading config from: {}", config.display());
    let wf_config = WorkforceConfig::load(config)?;
    let pg_cfg = wf_config.postgres.clone().filter(|p| p.enabled).ok_or_else(|| {
        eyre::eyre!(
            "no [postgres] section (or enabled = false) in {}. Set `[postgres] enabled = true` with your connection details before running this command.",
            config.display()
        )
    })?;
    let sqlite_path = from.unwrap_or_else(|| default_path(&wf_config));
    if !sqlite_path.exists() {
        eyre::bail!(
            "source SQLite {label} database not found at {}",
            sqlite_path.display()
        );
    }
    let count = migrate(sqlite_path, pg_cfg)
        .await
        .with_context(|| format!("migrate-{label} failed"))?;
    println!("Migrated {count} {label} record(s) into Postgres.");
    println!(
        "\nNext step: set `[{label}] backend = \"postgres\"` in {} and restart the hub.",
        config.display()
    );
    Ok(())
}

#[cfg(test)]
mod serve_guard_tests {
    /// `serve` refuses the artifact index from vellev when whether it was
    /// promoted cannot be read: never served on facts unread.
    #[tokio::test]
    async fn serve_refuses_vellev_artifacts_on_facts_it_cannot_read() {
        let mut config = workforce_common::settings::WorkforceConfig::default();
        config.artifacts.index_backend = "vellev".into();
        config.postgres = Some(workforce_common::postgres::PostgresConfig {
            enabled: true,
            url: Some("postgres://nobody@127.0.0.1:1/none".into()),
            connect_timeout_secs: 1,
            ..Default::default()
        });
        let refused = super::guard_before_serving(&config)
            .await
            .expect_err("refused");
        assert!(
            format!("{refused:#}").contains("is not served from vellev"),
            "{refused:#}"
        );
        config.artifacts.index_backend = "weaviate".into();
        super::guard_before_serving(&config).await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_are_reindexed_by_their_own_command() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["workforce", "artifacts", "reindex", "--config", "hub.toml"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Artifacts {
                command: ArtifactsCommand::Reindex { config, mirror: false, embed }
            } if config == std::path::Path::new("hub.toml")
                && embed.embed_url.is_none() && embed.seeding().unwrap().is_none()
        ));
        // The shadow's vellev mirror, by its own flag (DW-676).
        let cli = Cli::try_parse_from(["workforce", "artifacts", "reindex", "--mirror"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Artifacts {
                command: ArtifactsCommand::Reindex { mirror: true, .. }
            }
        ));
        let cli = Cli::try_parse_from([
            "workforce",
            "code",
            "reindex-vectors",
            "--repo",
            "site-1a2b",
            "--embed-url",
            "http://tei:8080",
            "--embed-concurrency",
            "16",
            "--force",
        ])
        .unwrap();
        let Commands::Code {
            command:
                CodeCommand::ReindexVectors {
                    config,
                    repo,
                    data_dir,
                    rate,
                    force,
                    take_over,
                    repos_at_once,
                    embed,
                },
        } = cli.command
        else {
            panic!("the code command");
        };
        assert_eq!((force, take_over, repos_at_once), (true, false, None));
        assert_eq!(config, std::path::Path::new("workforce.toml"));
        assert_eq!(
            (repo.as_deref(), rate, data_dir),
            (Some("site-1a2b"), None, None)
        );
        let seeding = embed.seeding().unwrap().expect("an outside embedder");
        assert_eq!((seeding.batch, seeding.concurrency), (32, 16));
        assert!(seeding.embedder.name().contains("http://tei:8080"));
        let unkeyed = EmbedArgs {
            embed_key_env: Some("WORKFORCE_TEST_NO_SUCH_KEY".into()),
            ..embed.clone()
        };
        assert!(unkeyed.seeding().is_err(), "a key named and not set");
        // No more texts are with the endpoint at once than it is said to
        // hold: 16 requests of 256 would be 4096, and it holds 512.
        let large = EmbedArgs {
            embed_batch: 256,
            ..embed.clone()
        };
        assert_eq!(large.requests(), (256, 2));
        let roomy = EmbedArgs {
            embed_inputs_in_flight: 4096,
            ..large.clone()
        };
        assert_eq!(roomy.requests(), (256, 16));
        let huge = EmbedArgs {
            embed_batch: 1024,
            ..large
        };
        assert_eq!(huge.requests(), (1024, 1), "one request at least");
        let odd = EmbedArgs {
            embed_api: "grpc".into(),
            ..embed
        };
        assert!(odd.seeding().is_err());
    }
    use std::io::Write;

    /// The scaffolded AGENTS.md becomes an agent's operational prompt, so it
    /// must document action-dispatch tools in the callable `tool(action="…")`
    /// form — the space form is what makes models invent `notes_write_human`.
    #[test]
    fn scaffolded_identity_files_use_the_real_call_shape() {
        let dir = tempfile::tempdir().unwrap();
        write_identity_files(dir.path()).unwrap();
        let agents = std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap();
        assert_eq!(
            workforce_common::identity::space_form_violation(&agents),
            None,
            "{agents}"
        );
        assert!(agents.contains(r#"memory(action="recall")"#), "{agents}");
    }

    fn write_csv(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".csv").tempfile().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn parse_role_import_csv_promotes_rows_with_team_id_into_team_rules() {
        use workforce_permissions::types::{PermissionScope, PermissionState};

        let team_a = uuid::Uuid::new_v4();
        let team_b = uuid::Uuid::new_v4();
        let csv = format!(
            "name,role,team_id,tool_pattern,action,priority\n\
             dev-mem-allow,developer,{team_a},memory.*,allow,10\n\
             dev-vault-ask,developer,{team_b},vault.read,ask,20\n",
        );
        let f = write_csv(&csv);
        let plan = parse_role_import_csv(f.path()).unwrap();
        assert_eq!(plan.importable.len(), 2);
        assert!(plan.skipped.is_empty());
        let allow = plan
            .importable
            .iter()
            .find(|r| r.tool_pattern == "memory.*")
            .unwrap();
        assert_eq!(allow.scope, PermissionScope::Team);
        assert_eq!(allow.scope_id, team_a.to_string());
        assert_eq!(allow.state, PermissionState::Allow);
        let ask = plan
            .importable
            .iter()
            .find(|r| r.tool_pattern == "vault.read")
            .unwrap();
        assert_eq!(ask.state, PermissionState::Ask);
    }

    #[test]
    fn parse_role_import_csv_skips_rows_without_team_id() {
        let csv = "name,role,team_id,tool_pattern,action,priority\n\
                   needs-mapping,developer,,memory.*,allow,10\n";
        let f = write_csv(csv);
        let plan = parse_role_import_csv(f.path()).unwrap();
        assert!(plan.importable.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].name, "needs-mapping");
    }

    #[test]
    fn parse_role_import_csv_rejects_unknown_action() {
        let team = uuid::Uuid::new_v4();
        let csv = format!(
            "name,role,team_id,tool_pattern,action,priority\n\
             bogus,developer,{team},memory.*,whatever,10\n",
        );
        let f = write_csv(&csv);
        let err = parse_role_import_csv(f.path()).unwrap_err();
        assert!(err.to_string().contains("whatever"));
    }

    #[test]
    fn parse_role_import_csv_accepts_require_approval_alias() {
        let team = uuid::Uuid::new_v4();
        let csv = format!(
            "name,role,team_id,tool_pattern,action,priority\n\
             legacy,developer,{team},vault.write,require_approval,10\n",
        );
        let f = write_csv(&csv);
        let plan = parse_role_import_csv(f.path()).unwrap();
        assert_eq!(plan.importable.len(), 1);
        assert_eq!(
            plan.importable[0].state,
            workforce_permissions::types::PermissionState::Ask
        );
    }

    #[test]
    fn resolve_extract_model_uses_config_default_when_no_override() {
        let resolved = resolve_extract_model(None, "standard", "google");
        let id = resolved.expect("must return Some so the provider never sees None");
        assert!(!id.is_empty());
    }

    #[test]
    fn resolve_extract_model_routes_explicit_override() {
        let resolved = resolve_extract_model(Some("powerful"), "standard", "google");
        let id = resolved.expect("override must resolve");
        assert!(!id.is_empty());
    }

    #[test]
    fn resolve_extract_model_passes_concrete_model_through() {
        // Non-tier strings flow through tier resolution unchanged so failover
        // can dispatch on the raw provider:model form.
        let raw = "openrouter:anthropic:claude-sonnet-4-5";
        let resolved = resolve_extract_model(Some(raw), "standard", "google");
        assert_eq!(resolved.as_deref(), Some(raw));
    }

    #[test]
    fn resolve_extract_model_falls_back_to_concrete_default() {
        // If the config default is a concrete model id (not a tier alias),
        // it must still pass through to the provider unchanged.
        let resolved = resolve_extract_model(None, "google/gemini-2.5-flash", "google");
        assert_eq!(resolved.as_deref(), Some("google/gemini-2.5-flash"));
    }

    #[test]
    fn resolve_extract_model_treats_empty_explicit_as_unset() {
        // `--model ""` is the same shape of bug as omitting the flag: an empty
        // spec reaches the provider as nothing-in-particular and 400s. Guard
        // by falling back to the config default.
        let resolved = resolve_extract_model(Some(""), "standard", "google");
        let id = resolved.expect("must return Some");
        assert!(!id.is_empty());
        // Whitespace-only behaves the same.
        let resolved = resolve_extract_model(Some("   "), "standard", "google");
        let id = resolved.expect("must return Some");
        assert!(!id.is_empty());
    }
}
