//! `workforce-local-mcp` CLI subcommands.
//!
//! Invoking the binary without a subcommand runs the MCP stdio server (the
//! default Claude Code wiring). `config <adapter>` / `config show` /
//! `config clear` manage adapter credentials in the OS keychain (macOS /
//! Windows) or the file-backed secret store (Linux).
//!
//! ## Single vs multi-workspace
//!
//! Each adapter can hold credentials for any number of workspaces /
//! accounts / instances (Linear / Jira call them workspaces, Datadog calls
//! them accounts, Metabase calls them instances — the CLI surfaces them all
//! as `--workspace` for consistency). A workspace name is any non-empty
//! short string chosen by the user (e.g. `work`, `personal`, `acme`).
//!
//! Writes:
//! - `config <adapter> --workspace <name>` →
//!   `skills/<adapter>/workspaces/<name>/<field>` plus an update to
//!   `skills/<adapter>/workspaces_index` (a JSON array of configured names).
//! - `config <adapter>` with no `--workspace` → legacy flat paths
//!   `skills/<adapter>/<field>`. Preserved for backward compatibility; the
//!   loader treats legacy fields as an implicit `default` workspace at read
//!   time (see [`crate::adapters`]).
//!
//! Setting a default workspace:
//! - `config <adapter> --set-default <name>` writes
//!   `skills/<adapter>/default_workspace`. When a tool call doesn't specify
//!   `workspace`/`account`/`instance`, the adapter uses this name if it is
//!   configured, else a workspace named `default`, else the sole workspace.

use std::collections::{BTreeSet, HashMap};

use clap::{Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr};
use workforce_common::settings::SecretStore;

use crate::hubs::{self, HubEntry};
use crate::paths;
use crate::secret_store::{EntryStatus, LocalSecretStore};

// ── Adapter schema ──────────────────────────────────────────────────────────
//
// One spec per adapter lists the fields the CLI stores. Ordering matters for
// display and for the interactive prompt order when a value isn't supplied
// via a flag. Secret fields are redacted to `****` in `config show`.

/// Every field the CLI writes per workspace for a given adapter.
struct AdapterSpec {
    /// Lowercase ID used in paths and subcommand names: `"jira"`, `"linear"`,
    /// `"datadog"`, `"metabase"`.
    name: &'static str,
    /// Capitalized name for user-facing messages.
    display: &'static str,
    /// Ordered list of field names stored per workspace.
    fields: &'static [&'static str],
    /// Subset of `fields` that hold secrets — these get redacted in `show`
    /// output.
    secret_fields: &'static [&'static str],
}

impl AdapterSpec {
    fn is_secret(&self, field: &str) -> bool {
        self.secret_fields.contains(&field)
    }
}

const JIRA: AdapterSpec = AdapterSpec {
    name: "jira",
    display: "Jira",
    fields: &["email", "site", "api_token"],
    secret_fields: &["api_token"],
};
const LINEAR: AdapterSpec = AdapterSpec {
    name: "linear",
    display: "Linear",
    fields: &["api_key", "default_team_id"],
    secret_fields: &["api_key"],
};
const DATADOG: AdapterSpec = AdapterSpec {
    name: "datadog",
    display: "Datadog",
    fields: &["api_key", "app_key", "site"],
    secret_fields: &["api_key", "app_key"],
};
const METABASE: AdapterSpec = AdapterSpec {
    name: "metabase",
    display: "Metabase",
    fields: &["site_url", "api_key", "max_query_rows"],
    secret_fields: &["api_key"],
};
const POSTMARK: AdapterSpec = AdapterSpec {
    name: "postmark",
    display: "Postmark",
    fields: &["server_token", "account_token", "base_url"],
    secret_fields: &["server_token", "account_token"],
};
const SENDGRID: AdapterSpec = AdapterSpec {
    name: "sendgrid",
    display: "SendGrid",
    fields: &["api_key", "from_email", "from_name", "base_url"],
    secret_fields: &["api_key"],
};
const XERO: AdapterSpec = AdapterSpec {
    name: "xero",
    display: "Xero",
    fields: &["client_id", "client_secret", "tenant_id"],
    secret_fields: &["client_id", "client_secret"],
};

const ADAPTERS: &[&AdapterSpec] = &[
    &JIRA, &LINEAR, &DATADOG, &METABASE, &POSTMARK, &SENDGRID, &XERO,
];

// ── Clap surface ────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(name = "workforce-local-mcp")]
#[command(about = "Local Claude Code MCP server for workforce capabilities")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Configure adapter credentials in the OS keychain / secret store.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set Jira Cloud credentials.
    Jira {
        /// Atlassian account email.
        #[arg(long)]
        email: Option<String>,
        /// Atlassian site URL, e.g. https://yoursite.atlassian.net.
        #[arg(long)]
        site: Option<String>,
        /// Save credentials under this named workspace. When omitted, writes
        /// to the legacy flat path (implicit `default` workspace).
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer (used when a tool
        /// call omits `workspace`). Independent of `--workspace`; can be
        /// combined, or used on its own to switch default without changing
        /// credentials.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set Linear credentials.
    Linear {
        /// Default team UUID (used by `linear.issue_create` when `team_id` is
        /// omitted).
        #[arg(long)]
        default_team_id: Option<String>,
        /// Save credentials under this named workspace. See `config jira` for
        /// full semantics.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set Datadog credentials.
    Datadog {
        /// Datadog site key (us1 / us3 / us5 / eu1 / ap1). Default: us1.
        #[arg(long)]
        site: Option<String>,
        /// Save credentials under this named workspace. Internally maps to
        /// Datadog's `accounts` map in the skill config.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set Metabase credentials.
    Metabase {
        /// Metabase site URL, e.g. https://mb.example.com.
        #[arg(long)]
        site_url: Option<String>,
        /// Cap on rows returned by query tools. Default 2000 (hard max).
        #[arg(long)]
        max_query_rows: Option<u64>,
        /// Save credentials under this named workspace. Internally maps to
        /// Metabase's `instances` map in the skill config.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set Postmark credentials.
    Postmark {
        /// Optional Postmark Account API token (X-Postmark-Account-Token).
        /// When omitted you are prompted with an option to skip. Only needed
        /// for `postmark.servers_list`, `postmark.domains_list`,
        /// `postmark.domain_get`, and `postmark.api` with
        /// `token_type=account`.
        #[arg(long)]
        account_token: Option<String>,
        /// Override the Postmark base URL. Default is
        /// `https://api.postmarkapp.com`. Only set for staging/sandbox.
        #[arg(long)]
        base_url: Option<String>,
        /// Save credentials under this named workspace. See `config jira`
        /// for full semantics.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set SendGrid credentials. Always prompts for the API key; the flags
    /// store the optional default sender and base URL alongside it.
    Sendgrid {
        /// Default From address for sends (must be a verified sender).
        #[arg(long)]
        from_email: Option<String>,
        /// Display name for the default From address.
        #[arg(long)]
        from_name: Option<String>,
        /// Override the API base URL, e.g. `https://api.eu.sendgrid.com`.
        #[arg(long)]
        base_url: Option<String>,
        /// Save credentials under this named workspace. See `config jira`
        /// for full semantics.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default workspace pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Set Xero Custom Connection credentials (client-credentials grant).
    Xero {
        /// Optional fixed tenant id. When omitted it is discovered from the
        /// connection on first use.
        #[arg(long)]
        tenant_id: Option<String>,
        /// Save credentials under this named entity (e.g. harbor, beacon).
        /// Internally maps to Xero's `entities` map in the skill config.
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
        /// Set the adapter's default entity pointer. See `config jira`.
        #[arg(long, value_name = "NAME")]
        set_default: Option<String>,
    },

    /// Register remote workforce hubs for the `wf.*` tools.
    #[command(subcommand)]
    Workforce(WorkforceCommand),

    /// Show which credentials are configured, grouped by adapter and
    /// workspace, plus the registered workforce hubs. Secrets are
    /// redacted as `****`.
    Show,

    /// Remove stored credentials for one adapter, or all of them.
    Clear {
        target: ClearTarget,
        /// Clear only this workspace instead of the whole adapter. Ignored
        /// for `all`. When omitted, wipes legacy flat paths, every named
        /// workspace, the index, and the default_workspace pointer.
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkforceCommand {
    /// Register a hub (or update its URL / token). The first hub registered
    /// becomes the active one.
    Add {
        /// Short name used with `wf.use`, e.g. driftwerk.
        name: String,
        /// The hub's public URL, e.g. https://workforce.example.com.
        #[arg(long)]
        url: String,
        /// Remote MCP token minted by `cmd: remote-mcp token` or the hub's
        /// "Remote MCP tokens" settings card. Prompted (hidden) when omitted.
        #[arg(long)]
        token: Option<String>,
    },
    /// Forget a hub.
    Remove { name: String },
    /// List registered hubs.
    List,
    /// Make a hub the target of the `wf.*` tools. A running server switches
    /// live via the `wf.use` tool; otherwise restart it.
    Use { name: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ClearTarget {
    Jira,
    Linear,
    Datadog,
    Metabase,
    Postmark,
    Sendgrid,
    Xero,
    /// Every registered workforce hub and its token.
    Workforce,
    All,
}

impl ClearTarget {
    fn adapter(self) -> Option<&'static AdapterSpec> {
        match self {
            Self::Jira => Some(&JIRA),
            Self::Linear => Some(&LINEAR),
            Self::Datadog => Some(&DATADOG),
            Self::Metabase => Some(&METABASE),
            Self::Postmark => Some(&POSTMARK),
            Self::Sendgrid => Some(&SENDGRID),
            Self::Xero => Some(&XERO),
            Self::Workforce | Self::All => None,
        }
    }
}

impl Cli {
    /// Entry point for CLI subcommands. Returns `Ok(true)` when a subcommand
    /// was run (caller should exit); `Ok(false)` when no subcommand was
    /// given and the caller should fall through to starting the MCP server.
    pub fn dispatch(self) -> Result<bool> {
        let Some(command) = self.command else {
            return Ok(false);
        };
        let store = LocalSecretStore::new();
        match command {
            Command::Config(cmd) => run_config(&store, cmd)?,
        }
        Ok(true)
    }
}

fn run_config(store: &LocalSecretStore, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Jira {
            email,
            site,
            workspace,
            set_default,
        } => configure_jira(store, workspace, set_default, email, site),
        ConfigCommand::Linear {
            default_team_id,
            workspace,
            set_default,
        } => configure_linear(store, workspace, set_default, default_team_id),
        ConfigCommand::Datadog {
            site,
            workspace,
            set_default,
        } => configure_datadog(store, workspace, set_default, site),
        ConfigCommand::Metabase {
            site_url,
            max_query_rows,
            workspace,
            set_default,
        } => configure_metabase(store, workspace, set_default, site_url, max_query_rows),
        ConfigCommand::Postmark {
            account_token,
            base_url,
            workspace,
            set_default,
        } => configure_postmark(store, workspace, set_default, account_token, base_url),
        ConfigCommand::Sendgrid {
            from_email,
            from_name,
            base_url,
            workspace,
            set_default,
        } => configure_sendgrid(
            store,
            workspace,
            set_default,
            from_email,
            from_name,
            base_url,
        ),
        ConfigCommand::Xero {
            tenant_id,
            workspace,
            set_default,
        } => configure_xero(store, workspace, set_default, tenant_id),
        ConfigCommand::Workforce(cmd) => configure_workforce(store, cmd),
        ConfigCommand::Show => show_config(store),
        ConfigCommand::Clear { target, workspace } => clear_config(store, target, workspace),
    }
}

// ── Workforce hubs ──────────────────────────────────────────────────────────

fn configure_workforce(store: &LocalSecretStore, command: WorkforceCommand) -> Result<()> {
    match command {
        WorkforceCommand::Add { name, url, token } => {
            let entry = HubEntry {
                name: hubs::validate_hub_name(&name)?.to_string(),
                url: hubs::normalise_hub_url(&url)?,
                token: hubs::validate_hub_token(&resolve_value(
                    token,
                    "Remote MCP token (wfr_…)",
                    true,
                )?)?
                .to_string(),
            };
            hubs::save_hub(store, &entry)?;
            let active = hubs::active_hub_name(store).as_deref() == Some(entry.name.as_str());
            println!(
                "Hub \"{}\" registered at {}{}. Call `wf.use` in a running session or restart the MCP server.",
                entry.name,
                entry.url,
                if active { " (active)" } else { "" },
            );
        }
        WorkforceCommand::Remove { name } => {
            if hubs::remove_hub(store, &name)? {
                println!("Hub \"{name}\" removed.");
            } else {
                println!("No hub named \"{name}\".");
            }
        }
        WorkforceCommand::List => print_hubs(store)?,
        WorkforceCommand::Use { name } => {
            hubs::set_active_hub(store, &name)?;
            println!("Active hub set to \"{name}\". Call `wf.use` in a running session or restart the MCP server.");
        }
    }
    Ok(())
}

fn print_hubs(store: &LocalSecretStore) -> Result<()> {
    let hubs = hubs::list_hubs(store)?;
    if hubs.is_empty() {
        println!("No workforce hubs registered.");
        println!("Run `workforce-local-mcp config workforce add <name> --url <url>` to add one.");
        return Ok(());
    }
    let active = hubs::active_hub_name(store);
    println!(
        "Workforce hubs  (active: {})",
        active.as_deref().unwrap_or("none")
    );
    for hub in hubs {
        let marker = if active.as_deref() == Some(hub.name.as_str()) {
            "*"
        } else {
            " "
        };
        println!("  {marker} {:15} {}", hub.name, hub.url);
    }
    Ok(())
}

// ── Per-adapter configure_* ─────────────────────────────────────────────────
//
// Each collects field values (from flags or interactive prompts), writes
// them via `save_adapter`, and optionally updates the default_workspace
// pointer. `--set-default <name>` on its own (no credential flags and an
// interactive terminal) still works — the user can skip credential prompts
// by omitting flags and running a command like
// `echo | workforce-local-mcp config linear --set-default work`. In
// practice the common case is `--workspace X --set-default X`, which both
// saves and switches the default.

fn configure_jira(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    email: Option<String>,
    site: Option<String>,
) -> Result<()> {
    // Allow `--set-default` without --workspace to just update the pointer.
    if workspace.is_none() && set_default.is_some() && email.is_none() && site.is_none() {
        return apply_set_default(store, &JIRA, set_default.as_deref());
    }
    let fields = vec![
        ("email", resolve_value(email, "Atlassian email", false)?),
        (
            "site",
            resolve_value(site, "Atlassian site URL", false)?
                .trim_end_matches('/')
                .to_string(),
        ),
        ("api_token", resolve_value(None, "Jira API token", true)?),
    ];
    save_adapter(store, &JIRA, workspace.as_deref(), fields)?;
    apply_set_default(store, &JIRA, set_default.as_deref())
}

fn configure_linear(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    default_team_id: Option<String>,
) -> Result<()> {
    if workspace.is_none() && set_default.is_some() && default_team_id.is_none() {
        return apply_set_default(store, &LINEAR, set_default.as_deref());
    }
    let mut fields = vec![("api_key", resolve_value(None, "Linear API key", true)?)];
    if let Some(team_id) = default_team_id {
        fields.push(("default_team_id", team_id));
    }
    save_adapter(store, &LINEAR, workspace.as_deref(), fields)?;
    apply_set_default(store, &LINEAR, set_default.as_deref())
}

fn configure_datadog(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    site: Option<String>,
) -> Result<()> {
    if workspace.is_none() && set_default.is_some() && site.is_none() {
        return apply_set_default(store, &DATADOG, set_default.as_deref());
    }
    let mut fields = vec![
        ("api_key", resolve_value(None, "Datadog API key", true)?),
        (
            "app_key",
            resolve_value(None, "Datadog Application key", true)?,
        ),
    ];
    if let Some(site) = site {
        fields.push(("site", site));
    }
    save_adapter(store, &DATADOG, workspace.as_deref(), fields)?;
    apply_set_default(store, &DATADOG, set_default.as_deref())
}

fn configure_metabase(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    site_url: Option<String>,
    max_query_rows: Option<u64>,
) -> Result<()> {
    if workspace.is_none()
        && set_default.is_some()
        && site_url.is_none()
        && max_query_rows.is_none()
    {
        return apply_set_default(store, &METABASE, set_default.as_deref());
    }
    let mut fields = vec![
        (
            "site_url",
            resolve_value(site_url, "Metabase site URL", false)?
                .trim_end_matches('/')
                .to_string(),
        ),
        ("api_key", resolve_value(None, "Metabase API key", true)?),
    ];
    if let Some(cap) = max_query_rows {
        fields.push(("max_query_rows", cap.to_string()));
    }
    save_adapter(store, &METABASE, workspace.as_deref(), fields)?;
    apply_set_default(store, &METABASE, set_default.as_deref())
}

fn configure_postmark(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    account_token_flag: Option<String>,
    base_url: Option<String>,
) -> Result<()> {
    // `--set-default NAME` on its own just updates the default pointer —
    // same pattern as the other adapters.
    if workspace.is_none()
        && set_default.is_some()
        && account_token_flag.is_none()
        && base_url.is_none()
    {
        return apply_set_default(store, &POSTMARK, set_default.as_deref());
    }
    let server_token = resolve_value(None, "Postmark Server Token", true)?;
    let account_token = resolve_optional_value(
        account_token_flag,
        "Postmark Account Token (optional, press Enter to skip)",
        true,
    )?;
    let mut fields = vec![("server_token", server_token)];
    if let Some(tok) = account_token {
        fields.push(("account_token", tok));
    }
    if let Some(url) = base_url {
        fields.push(("base_url", url.trim_end_matches('/').to_string()));
    }
    save_adapter(store, &POSTMARK, workspace.as_deref(), fields)?;
    apply_set_default(store, &POSTMARK, set_default.as_deref())
}

fn configure_sendgrid(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    from_email: Option<String>,
    from_name: Option<String>,
    base_url: Option<String>,
) -> Result<()> {
    let only_default = [&from_email, &from_name, &base_url]
        .iter()
        .all(|flag| flag.is_none());
    if workspace.is_none() && set_default.is_some() && only_default {
        return apply_set_default(store, &SENDGRID, set_default.as_deref());
    }
    let api_key = resolve_value(None, "SendGrid API Key", true)?;
    let fields = std::iter::once(("api_key", api_key))
        .chain(from_email.map(|v| ("from_email", v)))
        .chain(from_name.map(|v| ("from_name", v)))
        .chain(base_url.map(|v| ("base_url", v.trim_end_matches('/').to_string())))
        .collect();
    save_adapter(store, &SENDGRID, workspace.as_deref(), fields)?;
    apply_set_default(store, &SENDGRID, set_default.as_deref())
}

fn configure_xero(
    store: &LocalSecretStore,
    workspace: Option<String>,
    set_default: Option<String>,
    tenant_id: Option<String>,
) -> Result<()> {
    if workspace.is_none() && set_default.is_some() && tenant_id.is_none() {
        return apply_set_default(store, &XERO, set_default.as_deref());
    }
    let mut fields = vec![
        ("client_id", resolve_value(None, "Xero Client ID", true)?),
        (
            "client_secret",
            resolve_value(None, "Xero Client Secret", true)?,
        ),
    ];
    if let Some(tid) = tenant_id {
        fields.push(("tenant_id", tid));
    }
    save_adapter(store, &XERO, workspace.as_deref(), fields)?;
    apply_set_default(store, &XERO, set_default.as_deref())
}

// ── Write helpers ───────────────────────────────────────────────────────────

/// Persist `(field, value)` pairs under either
/// `skills/<adapter>/<field>` (legacy) or
/// `skills/<adapter>/workspaces/<workspace>/<field>`, and update the
/// workspaces index when a named workspace is written.
fn save_adapter(
    store: &LocalSecretStore,
    spec: &AdapterSpec,
    workspace: Option<&str>,
    fields: Vec<(&str, String)>,
) -> Result<()> {
    match workspace {
        Some(ws) => {
            let ws = validate_workspace_name(ws)?;
            for (field, value) in &fields {
                store.set(&paths::workspace_field(spec.name, ws, field), value)?;
            }
            add_to_index(store, spec, ws)?;
            println!(
                "{} credentials saved under workspace \"{ws}\". Restart the MCP server to pick them up.",
                spec.display,
            );
        }
        None => {
            for (field, value) in &fields {
                store.set(&paths::legacy_field(spec.name, field), value)?;
            }
            println!(
                "{} credentials saved. Restart the MCP server to pick them up.",
                spec.display,
            );
        }
    }
    Ok(())
}

/// Add `workspace` to the adapter's index if not already present. The index
/// is a JSON array; we keep it sorted for deterministic output.
fn add_to_index(store: &LocalSecretStore, spec: &AdapterSpec, workspace: &str) -> Result<()> {
    let path = paths::workspaces_index(spec.name);
    let mut current = store
        .get_string_list(&path)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let inserted = current.insert(workspace.to_string());
    if inserted {
        let items: Vec<String> = current.into_iter().collect();
        store.set_string_list(&path, &items)?;
    }
    Ok(())
}

fn remove_from_index(
    store: &LocalSecretStore,
    spec: &AdapterSpec,
    workspace: &str,
) -> Result<bool> {
    let path = paths::workspaces_index(spec.name);
    let mut current = store
        .get_string_list(&path)
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let removed = current.remove(workspace);
    if removed {
        let items: Vec<String> = current.into_iter().collect();
        if items.is_empty() {
            store.delete(&path)?;
        } else {
            store.set_string_list(&path, &items)?;
        }
    }
    Ok(removed)
}

fn apply_set_default(
    store: &LocalSecretStore,
    spec: &AdapterSpec,
    set_default: Option<&str>,
) -> Result<()> {
    let Some(name) = set_default else {
        return Ok(());
    };
    let name = validate_workspace_name(name)?;
    store.set(&paths::default_workspace(spec.name), name)?;
    println!("{} default workspace set to \"{name}\".", spec.display);
    Ok(())
}

/// Reject workspace names that would break the on-disk path scheme: empty
/// after trimming, or containing a path separator that would collide with
/// the `skills/<adapter>/workspaces/<name>/<field>` structure.
fn validate_workspace_name(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(eyre::eyre!("workspace name cannot be empty"));
    }
    if trimmed.contains('/') {
        return Err(eyre::eyre!(
            "workspace name cannot contain '/' (got {trimmed:?})"
        ));
    }
    Ok(trimmed)
}

// ── show ────────────────────────────────────────────────────────────────────

/// Snapshot of one adapter's stored secret layout, built by polling the
/// store for every known path. `AccessDenied` entries are surfaced
/// separately so the remediation hint in `show_config` can name the
/// adapters whose entries are ACL-locked.
struct AdapterSnapshot {
    workspaces: Vec<WorkspaceSnapshot>,
    /// Fields found at legacy flat paths, used only if no explicit
    /// `default` workspace exists. Preserves the field order from
    /// `AdapterSpec::fields` so display is stable.
    legacy_fields: Vec<(String, String)>,
    default_workspace: Option<String>,
    /// Paths that exist in the keychain but the current binary can't read —
    /// macOS ACL-lock case. The CLI surfaces these as a separate warning
    /// block; they otherwise get silently dropped.
    denied_paths: Vec<String>,
    errors: Vec<String>,
}

struct WorkspaceSnapshot {
    name: String,
    fields: Vec<(String, String)>,
}

/// The workspace a call targets when it names none, by the adapters' rule
/// over the configured names; legacy flat fields count as `default`.
fn default_label(snap: &AdapterSnapshot) -> String {
    let names: HashMap<String, ()> = snap
        .workspaces
        .iter()
        .map(|ws| ws.name.as_str())
        .chain((!snap.legacy_fields.is_empty()).then_some("default"))
        .map(|name| (name.to_string(), ()))
        .collect();
    workforce_skills::adapters::utils::preferred_instance(&names, snap.default_workspace.as_deref())
        .unwrap_or("none")
        .to_string()
}

fn show_config(store: &LocalSecretStore) -> Result<()> {
    let snapshots: Vec<(&&AdapterSpec, AdapterSnapshot)> = ADAPTERS
        .iter()
        .map(|spec| (spec, snapshot_adapter(store, spec)))
        .collect();

    // Surface read errors first, then ACL-denied entries. Then the per-
    // adapter layout. Same ordering as the single-workspace version so
    // existing users' eyes find the remediation block first.
    for msg in snapshots.iter().flat_map(|(_, s)| &s.errors) {
        eprintln!("warning: {msg}");
    }

    let all_denied: Vec<&String> = snapshots
        .iter()
        .flat_map(|(_, s)| &s.denied_paths)
        .collect();
    if !all_denied.is_empty() {
        let denied_adapters: Vec<&str> = snapshots
            .iter()
            .filter(|(_, s)| !s.denied_paths.is_empty())
            .map(|(spec, _)| spec.name)
            .collect();
        eprintln!(
            "{} keychain entr{} exist but the current binary can't read them:",
            all_denied.len(),
            if all_denied.len() == 1 { "y" } else { "ies" }
        );
        for p in &all_denied {
            eprintln!("  {p}");
        }
        eprintln!();
        eprintln!("This usually means a differently-signed binary (e.g. an older install.sh");
        eprintln!("build) wrote them and this build isn't on the keychain ACL. Re-run:");
        eprintln!();
        for adapter in &denied_adapters {
            eprintln!("  workforce-local-mcp config {adapter} [--workspace <name>]");
        }
        eprintln!();
        eprintln!("to rewrite those entries with the current binary's identity. Click");
        eprintln!("\"Always Allow\" on each Keychain prompt.");
        eprintln!();
    }

    let any_printed = snapshots
        .iter()
        .any(|(_, s)| !s.workspaces.is_empty() || !s.legacy_fields.is_empty());
    if !any_printed && all_denied.is_empty() {
        println!("No adapter credentials configured.");
        println!(
            "Run `workforce-local-mcp config <jira|linear|datadog|metabase> [--workspace NAME]` to set them."
        );
        println!();
    }

    for (spec, snap) in &snapshots {
        if snap.workspaces.is_empty() && snap.legacy_fields.is_empty() {
            continue;
        }
        println!("{}  (default: {})", spec.display, default_label(snap));
        print_adapter_rows(spec, snap);
        println!();
    }

    print_hubs(store)
}

fn print_adapter_rows(spec: &AdapterSpec, snap: &AdapterSnapshot) {
    // Legacy fields show as a `default` row, but only when no explicit
    // `default` workspace already exists — the loader applies the same
    // precedence, so the display matches runtime behavior.
    let has_explicit_default = snap.workspaces.iter().any(|ws| ws.name == "default");
    if !snap.legacy_fields.is_empty() && !has_explicit_default {
        println!(
            "  default (legacy) {}",
            format_fields(spec, &snap.legacy_fields)
        );
    }
    for ws in &snap.workspaces {
        println!("  {:15} {}", ws.name, format_fields(spec, &ws.fields));
    }
}

fn format_fields(spec: &AdapterSpec, fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(name, value)| {
            let shown = if spec.is_secret(name) {
                redact(value)
            } else {
                value.clone()
            };
            format!("{name}={shown}")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn snapshot_adapter(store: &LocalSecretStore, spec: &AdapterSpec) -> AdapterSnapshot {
    let mut denied_paths = Vec::new();
    let mut errors = Vec::new();

    // Index → list of workspace names. Index itself can be denied on macOS.
    let mut workspace_names: Vec<String> = {
        let index_path = paths::workspaces_index(spec.name);
        match store.probe(&index_path) {
            EntryStatus::Present(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                errors.push(format!("parsing {index_path}: {e}"));
                Vec::new()
            }),
            EntryStatus::NotSet => Vec::new(),
            EntryStatus::AccessDenied => {
                denied_paths.push(index_path);
                Vec::new()
            }
            EntryStatus::Error(msg) => {
                errors.push(msg);
                Vec::new()
            }
        }
    };
    workspace_names.sort();
    workspace_names.dedup();

    let mut workspaces = Vec::with_capacity(workspace_names.len());
    for name in workspace_names {
        let mut fields = Vec::new();
        for &field in spec.fields {
            let path = paths::workspace_field(spec.name, &name, field);
            match store.probe(&path) {
                EntryStatus::Present(v) => fields.push((field.to_string(), v)),
                EntryStatus::NotSet => {}
                EntryStatus::AccessDenied => denied_paths.push(path),
                EntryStatus::Error(msg) => errors.push(msg),
            }
        }
        if !fields.is_empty() {
            workspaces.push(WorkspaceSnapshot { name, fields });
        }
    }

    let mut legacy_fields = Vec::new();
    for &field in spec.fields {
        let path = paths::legacy_field(spec.name, field);
        match store.probe(&path) {
            EntryStatus::Present(v) => legacy_fields.push((field.to_string(), v)),
            EntryStatus::NotSet => {}
            EntryStatus::AccessDenied => denied_paths.push(path),
            EntryStatus::Error(msg) => errors.push(msg),
        }
    }

    let default_workspace = store.get(&paths::default_workspace(spec.name));

    AdapterSnapshot {
        workspaces,
        legacy_fields,
        default_workspace,
        denied_paths,
        errors,
    }
}

// ── clear ───────────────────────────────────────────────────────────────────

fn clear_config(
    store: &LocalSecretStore,
    target: ClearTarget,
    workspace: Option<String>,
) -> Result<()> {
    match (target.adapter(), workspace) {
        (Some(spec), Some(ws)) => {
            let removed = clear_workspace(store, spec, &ws)?;
            println!(
                "Cleared {removed} entries for {} workspace \"{ws}\".",
                spec.display
            );
        }
        (Some(spec), None) => {
            let removed = clear_adapter(store, spec)?;
            println!("Cleared {removed} entries for {}.", spec.display);
        }
        (None, _) if matches!(target, ClearTarget::Workforce) => {
            let removed = hubs::clear_hubs(store)?;
            println!("Cleared {removed} entries for workforce hubs.");
        }
        (None, _) => {
            let mut removed = hubs::clear_hubs(store)?;
            for spec in ADAPTERS {
                removed += clear_adapter(store, spec)?;
            }
            println!("Cleared {removed} entries across all adapters and workforce hubs.");
        }
    }
    Ok(())
}

fn clear_adapter(store: &LocalSecretStore, spec: &AdapterSpec) -> Result<usize> {
    let mut removed = 0;

    // Named workspaces first, via the index.
    let index_path = paths::workspaces_index(spec.name);
    let names: Vec<String> = store.get_string_list(&index_path).unwrap_or_default();
    for name in &names {
        for &field in spec.fields {
            let path = paths::workspace_field(spec.name, name, field);
            if store.delete(&path)? {
                removed += 1;
            }
        }
    }
    if store.delete(&index_path)? {
        removed += 1;
    }

    // Legacy flat fields.
    for &field in spec.fields {
        let path = paths::legacy_field(spec.name, field);
        if store.delete(&path)? {
            removed += 1;
        }
    }

    // default_workspace pointer.
    if store.delete(&paths::default_workspace(spec.name))? {
        removed += 1;
    }

    Ok(removed)
}

fn clear_workspace(store: &LocalSecretStore, spec: &AdapterSpec, workspace: &str) -> Result<usize> {
    let mut removed = 0;
    for &field in spec.fields {
        let path = paths::workspace_field(spec.name, workspace, field);
        if store.delete(&path)? {
            removed += 1;
        }
    }
    if remove_from_index(store, spec, workspace)? {
        removed += 1;
    }
    // If this was the declared default, clear the pointer too — leaving it
    // pointing at a deleted workspace would surface as silent
    // adapter-init failure at startup.
    if let Some(current_default) = store.get(&paths::default_workspace(spec.name)) {
        if current_default == workspace && store.delete(&paths::default_workspace(spec.name))? {
            removed += 1;
        }
    }
    Ok(removed)
}

// ── Value resolution (flag or interactive prompt) ───────────────────────────

/// Return `value` if present, otherwise prompt the user interactively. On a
/// terminal, secrets are read with `rpassword` (input hidden); when stdin is
/// piped we read a plain line, so `echo "$token" | ... config jira …` also
/// works for scripted setup.
fn resolve_value(value: Option<String>, label: &str, secret: bool) -> Result<String> {
    use std::io::IsTerminal;

    if let Some(v) = value.filter(|s| !s.is_empty()) {
        return Ok(v);
    }

    let raw = if secret && std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{label}: "))
            .wrap_err_with(|| format!("reading {label} from terminal"))?
    } else {
        print_prompt(label);
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .wrap_err_with(|| format!("reading {label} from stdin"))?;
        buf
    };

    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Err(eyre::eyre!("{label} is required"))
    } else {
        Ok(trimmed.to_string())
    }
}

/// Same as [`resolve_value`] but returns `Ok(None)` when the user provides no
/// input (empty line / empty flag value). Canonical pattern for optional
/// secrets like Postmark's `account_token`.
fn resolve_optional_value(
    value: Option<String>,
    label: &str,
    secret: bool,
) -> Result<Option<String>> {
    use std::io::IsTerminal;

    if let Some(v) = value {
        let trimmed = v.trim();
        return Ok(if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        });
    }

    let raw = if secret && std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{label}: "))
            .wrap_err_with(|| format!("reading {label} from terminal"))?
    } else {
        print_prompt(label);
        let mut buf = String::new();
        std::io::stdin()
            .read_line(&mut buf)
            .wrap_err_with(|| format!("reading {label} from stdin"))?;
        buf
    };

    let trimmed = raw.trim();
    Ok(if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    })
}

fn print_prompt(label: &str) {
    use std::io::Write;
    // Prompt on stderr so it stays out of piped stdout.
    let _ = write!(std::io::stderr(), "{label}: ");
    let _ = std::io::stderr().flush();
}

fn redact(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        "****".into()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(names: &[&str], legacy: bool, selector: Option<&str>) -> AdapterSnapshot {
        AdapterSnapshot {
            workspaces: names
                .iter()
                .map(|name| WorkspaceSnapshot {
                    name: name.to_string(),
                    fields: Vec::new(),
                })
                .collect(),
            legacy_fields: if legacy {
                vec![("api_key".to_string(), "k".to_string())]
            } else {
                Vec::new()
            },
            default_workspace: selector.map(str::to_string),
            denied_paths: Vec::new(),
            errors: Vec::new(),
        }
    }

    #[test]
    fn default_label_follows_the_adapters_rule() {
        assert_eq!(default_label(&snapshot(&["work"], false, None)), "work");
        assert_eq!(default_label(&snapshot(&["work"], true, None)), "default");
        assert_eq!(
            default_label(&snapshot(&["work", "home"], false, Some("home"))),
            "home"
        );
        assert_eq!(
            default_label(&snapshot(&["work", "home"], false, Some("gone"))),
            "none"
        );
    }

    #[test]
    fn every_adapter_has_exactly_one_clear_target() {
        let targets: Vec<&str> = ClearTarget::value_variants()
            .iter()
            .filter_map(|target| target.adapter())
            .map(|spec| spec.name)
            .collect();
        let adapters: Vec<&str> = ADAPTERS.iter().map(|spec| spec.name).collect();
        assert_eq!(targets, adapters);
    }

    #[test]
    fn redact_masks_non_empty() {
        assert_eq!(redact(""), "");
        assert_eq!(redact("super-secret-token"), "****");
    }

    #[test]
    fn adapter_spec_is_secret_exhaustive() {
        assert!(JIRA.is_secret("api_token"));
        assert!(!JIRA.is_secret("email"));
        assert!(!JIRA.is_secret("site"));
        assert!(LINEAR.is_secret("api_key"));
        assert!(!LINEAR.is_secret("default_team_id"));
        assert!(DATADOG.is_secret("api_key"));
        assert!(DATADOG.is_secret("app_key"));
        assert!(!DATADOG.is_secret("site"));
        assert!(METABASE.is_secret("api_key"));
        assert!(!METABASE.is_secret("site_url"));
        assert!(!METABASE.is_secret("max_query_rows"));
        assert!(POSTMARK.is_secret("server_token"));
        assert!(POSTMARK.is_secret("account_token"));
        assert!(!POSTMARK.is_secret("base_url"));
    }

    #[test]
    fn resolve_optional_value_respects_explicit_empty_flag() {
        // An explicit empty flag stays "not provided", not an error.
        let out = resolve_optional_value(Some("".to_string()), "label", false).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn resolve_optional_value_trims_and_returns_value() {
        let out =
            resolve_optional_value(Some("  token_123  ".to_string()), "label", false).unwrap();
        assert_eq!(out.as_deref(), Some("token_123"));
    }

    #[test]
    fn workspace_name_validation_rejects_empty_and_slashes() {
        assert!(validate_workspace_name("").is_err());
        assert!(validate_workspace_name("   ").is_err());
        assert!(validate_workspace_name("work/sub").is_err());
        assert_eq!(validate_workspace_name("work").unwrap(), "work");
        assert_eq!(validate_workspace_name("  work  ").unwrap(), "work");
    }

    // Linux-only integration tests run against a real LocalSecretStore
    // backed by a per-test tempfile. Uses the `with_path` test-only
    // constructor instead of the `WORKFORCE_LOCAL_MCP_SECRETS` env var so
    // tests stay hermetic under cargo's default parallel-test execution
    // (env vars are process-global and races would corrupt results).
    // macOS keychain tests would require user interaction for ACL grants
    // and aren't safe to run in parallel CI.
    #[cfg(target_os = "linux")]
    mod linux_integration {
        use super::*;

        fn with_tmp_store<F: FnOnce(&LocalSecretStore)>(f: F) {
            let dir = tempfile::tempdir().unwrap();
            let store = LocalSecretStore::with_path(dir.path().join("secrets.toml"));
            f(&store);
        }

        #[test]
        fn save_named_workspace_updates_index() {
            with_tmp_store(|store| {
                save_adapter(
                    store,
                    &LINEAR,
                    Some("work"),
                    vec![("api_key", "lin_work_key".to_string())],
                )
                .unwrap();
                save_adapter(
                    store,
                    &LINEAR,
                    Some("personal"),
                    vec![("api_key", "lin_personal_key".to_string())],
                )
                .unwrap();

                let index = store
                    .get_string_list(&paths::workspaces_index("linear"))
                    .unwrap();
                assert_eq!(index, vec!["personal".to_string(), "work".to_string()]);
                assert_eq!(
                    store.get(&paths::workspace_field("linear", "work", "api_key")),
                    Some("lin_work_key".to_string())
                );
                assert_eq!(
                    store.get(&paths::workspace_field("linear", "personal", "api_key")),
                    Some("lin_personal_key".to_string())
                );
            });
        }

        #[test]
        fn save_legacy_without_workspace_keeps_flat_paths_and_empty_index() {
            with_tmp_store(|store| {
                save_adapter(
                    store,
                    &LINEAR,
                    None,
                    vec![("api_key", "legacy_key".to_string())],
                )
                .unwrap();
                assert_eq!(
                    store.get(&paths::legacy_field("linear", "api_key")),
                    Some("legacy_key".to_string())
                );
                assert!(store
                    .get_string_list(&paths::workspaces_index("linear"))
                    .unwrap()
                    .is_empty());
            });
        }

        #[test]
        fn clear_workspace_removes_fields_and_index_entry() {
            with_tmp_store(|store| {
                save_adapter(store, &LINEAR, Some("work"), vec![("api_key", "k1".into())]).unwrap();
                save_adapter(
                    store,
                    &LINEAR,
                    Some("personal"),
                    vec![("api_key", "k2".into())],
                )
                .unwrap();
                apply_set_default(store, &LINEAR, Some("work")).unwrap();

                clear_workspace(store, &LINEAR, "personal").unwrap();
                let index = store
                    .get_string_list(&paths::workspaces_index("linear"))
                    .unwrap();
                assert_eq!(index, vec!["work".to_string()]);
                assert_eq!(
                    store.get(&paths::workspace_field("linear", "personal", "api_key")),
                    None
                );
                // Default pointer stays — `work` is still the default.
                assert_eq!(
                    store.get(&paths::default_workspace("linear")),
                    Some("work".to_string())
                );

                // Now clear the default one — pointer should also be dropped
                // so we don't point at a deleted workspace.
                clear_workspace(store, &LINEAR, "work").unwrap();
                assert!(store
                    .get_string_list(&paths::workspaces_index("linear"))
                    .unwrap()
                    .is_empty());
                assert_eq!(store.get(&paths::default_workspace("linear")), None);
            });
        }

        #[test]
        fn clear_adapter_wipes_everything() {
            with_tmp_store(|store| {
                save_adapter(store, &LINEAR, Some("work"), vec![("api_key", "k".into())]).unwrap();
                save_adapter(store, &LINEAR, None, vec![("api_key", "legacy".into())]).unwrap();
                apply_set_default(store, &LINEAR, Some("work")).unwrap();

                let removed = clear_adapter(store, &LINEAR).unwrap();
                // work/api_key + index + legacy api_key + default_workspace = 4
                assert_eq!(removed, 4);
                assert!(store
                    .get_string_list(&paths::workspaces_index("linear"))
                    .unwrap()
                    .is_empty());
                assert_eq!(store.get(&paths::legacy_field("linear", "api_key")), None);
                assert_eq!(store.get(&paths::default_workspace("linear")), None);
            });
        }

        #[test]
        fn snapshot_surfaces_both_legacy_and_named() {
            with_tmp_store(|store| {
                save_adapter(store, &LINEAR, None, vec![("api_key", "legacy_key".into())]).unwrap();
                save_adapter(
                    store,
                    &LINEAR,
                    Some("work"),
                    vec![("api_key", "work_key".into())],
                )
                .unwrap();
                apply_set_default(store, &LINEAR, Some("work")).unwrap();

                let snap = snapshot_adapter(store, &LINEAR);
                assert_eq!(snap.workspaces.len(), 1);
                assert_eq!(snap.workspaces[0].name, "work");
                assert_eq!(snap.legacy_fields.len(), 1);
                assert_eq!(snap.legacy_fields[0].0, "api_key");
                assert_eq!(snap.default_workspace.as_deref(), Some("work"));
            });
        }
    }
}
