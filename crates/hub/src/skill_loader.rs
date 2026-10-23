//! Config-driven built-in skill loader.
//!
//! Instead of hardcoding each skill's tools in `serve.rs`, this module
//! provides a single entry point that loads all built-in skills whose
//! configuration or credentials are available.
//!
//! **Two loading paths:**
//! - **Config/env var** (`load_builtin_skill_tools`): reads from `WorkforceConfig` / env vars.
//!   Used at startup for backward compatibility.
//! - **Vault** (`load_skill_from_vault`): reads credentials from the encrypted vault.
//!   Used by `system.enable_skill` at runtime and vault-first startup.
//!
//! Slack is intentionally NOT loaded here — it's wired through
//! `crate::slack_manager::SlackManager`, which discovers per-workspace
//! tokens (`slack.{workspace_id}.bot_token`) and registers receivers on
//! a different lifecycle from the skill loader.
//!
//! To add a new built-in skill:
//! 1. Add an entry to `builtin_skill_catalog()`
//! 2. Add a match arm in `load_skill_from_vault()`
//! 3. Optionally add a config/env path in `load_builtin_skill_tools()`

use std::sync::Arc;

use serde::Serialize;
use tracing::{info, warn};

use workforce_agent::llm::environment::SkillVariants;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::Tool;
use workforce_common::settings::WorkforceConfig;
use workforce_skills::adapters::auth::shared_get;

use crate::serve::AgentService;
use crate::vault::Vault;

// ─── Skill Catalog ───────────────────────────────────────────────────────────

/// Metadata about a built-in skill.
pub struct BuiltinSkillMeta {
    /// Canonical skill name (e.g., "github").
    pub name: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Vault paths where this skill's credentials should be stored.
    pub required_credentials: &'static [CredentialMeta],
    /// Tool name prefix — tools are named `{prefix}.xxx`.
    pub tool_prefix: &'static str,
    /// Setup instructions for the agent to guide users through configuration.
    pub setup_hint: &'static str,
    /// Vault prefix for multi-workspace discovery (e.g., "skills/github/workspaces/").
    /// Empty string if multi-workspace is not supported.
    pub workspace_vault_prefix: &'static str,
    /// Per-workspace credential template — key suffixes within the workspace prefix.
    pub workspace_credentials: &'static [WorkspaceCredentialMeta],
    /// Default storage scope for this skill's credentials (per-user / shared / either).
    pub default_scope: CredScope,
    /// Whether this skill's credentials are hand-entered or OAuth-managed.
    pub managed_by: CredManaged,
    /// Whether this skill supports a `config.toml` file that users can edit
    /// via the Configuration tab, even before the file has been created.
    pub supports_config: bool,
    /// Default `config.toml` content returned by the API when no file exists
    /// yet. Empty string means no template.
    pub default_config_template: &'static str,
}

impl BuiltinSkillMeta {
    /// Directory name this skill is stored under inside `skills_dir` — usually
    /// the canonical name, except Slack which lives in `slack-integration/`.
    pub fn dir_name(&self) -> &'static str {
        match self.name {
            "slack" => "slack-integration",
            other => other,
        }
    }
}

/// Metadata about a credential a skill needs.
pub struct CredentialMeta {
    /// Vault path (e.g., "skills/github/token").
    pub vault_path: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Whether this credential is required (vs. optional).
    pub required: bool,
}

/// Metadata about a per-workspace credential key suffix.
pub struct WorkspaceCredentialMeta {
    pub key: &'static str,
    pub description: &'static str,
    pub required: bool,
    /// Whether this key may be hand-entered via the vault editor. Runtime
    /// material written by an OAuth flow (access tokens, expiries) is set to
    /// `false`: recognised by the catalog so it is never badged unrecognised,
    /// but hidden from the "add credential" dropdown.
    pub addable: bool,
}

/// Whether a skill's credentials are stored per-user, shared across all users
/// of an agent, or either (user's choice). Drives the vault editor's scope
/// selector and the per-user write allow-list (see `api_handlers::my_settings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredScope {
    /// Always per-user (`users/{uid}/...`); shared storage is not meaningful.
    PerUser,
    /// Always shared across every user of the agent (bare path).
    Shared,
    /// User picks — defaults to per-user but may be set agent-shared.
    Either,
}

impl CredScope {
    pub fn as_str(self) -> &'static str {
        match self {
            CredScope::PerUser => "per_user",
            CredScope::Shared => "shared",
            CredScope::Either => "either",
        }
    }
    /// True when a per-user (`users/{uid}/...`) write is permitted for this scope.
    pub fn allows_per_user(self) -> bool {
        matches!(self, CredScope::PerUser | CredScope::Either)
    }
}

/// Whether a credential is hand-entered or maintained by an automated flow
/// (e.g. OAuth refresh). OAuth-managed credentials are risky to edit by hand —
/// the editor surfaces a warning and steers users to the setup flow instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredManaged {
    /// Hand-entered (API keys, tokens, hostnames).
    Manual,
    /// Written/rotated by an OAuth flow; manual edits may be overwritten.
    Oauth,
}

impl CredManaged {
    pub fn as_str(self) -> &'static str {
        match self {
            CredManaged::Manual => "manual",
            CredManaged::Oauth => "oauth",
        }
    }
}

/// Serializable skill info for the global skills API.
#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub dir_name: String,
    pub description: String,
    #[serde(rename = "type")]
    pub skill_type: String,
    pub has_config: bool,
    /// Whether this skill supports a `config.toml` editable via the UI,
    /// even when no file exists yet (a default template is provided).
    pub supports_config: bool,
    pub editable: bool,
    pub required_credentials: Vec<CredentialInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_vault_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_credentials: Vec<WorkspaceCredentialTemplate>,
}

/// Serializable credential info for the global skills API.
#[derive(Debug, Clone, Serialize)]
pub struct CredentialInfo {
    pub vault_path: String,
    pub description: String,
    pub required: bool,
}

/// Serializable per-workspace credential template.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceCredentialTemplate {
    pub key: String,
    pub description: String,
    pub required: bool,
}

/// A `{var}` segment in a catalog path template — the "Change-Me" the user fills.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogVariable {
    /// Variable name as it appears in the template (e.g. "workspace").
    pub name: String,
    /// Human label for the form field (e.g. "Workspace name").
    pub label: String,
    /// Placeholder text shown in the input (always "change-me").
    pub placeholder: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
}

/// One offerable credential path for the vault editor's path dropdown.
///
/// Flattens the per-skill catalog (and non-skill credential families) into a
/// single list the UI consumes via `GET /api/vault/catalog`. Fixed paths carry
/// an empty `variables`; templated paths (per-workspace/account/instance) carry
/// one `{var}` segment.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    /// Stable identifier — the template path itself.
    pub id: String,
    /// Owning skill name, or `None` for non-skill families (LLM accounts, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Group key for the dropdown (skill or family name).
    pub group: String,
    /// Display label for the option.
    pub label: String,
    /// Path template with `{var}` placeholders for variable segments.
    pub template: String,
    /// Always serialized (empty for fixed paths) — the UI dereferences
    /// `variables.map(...)`, so an omitted field would crash the form.
    #[serde(default)]
    pub variables: Vec<CatalogVariable>,
    /// Origin of this credential: "builtin" (a built-in skill / non-skill
    /// family) or "mcp" (an MCP server preset). Drives the built-in-vs-MCP
    /// distinction in the dropdown and grouped vault view.
    pub source: &'static str,
    /// Whether this path is offered in the "add credential" dropdown. Runtime
    /// OAuth material is recognised (so it is not badged) but not addable.
    pub addable: bool,
    /// Storage scope: "per_user" | "shared" | "either".
    pub scope: &'static str,
    /// "manual" | "oauth".
    pub managed_by: &'static str,
    /// Whether a non-admin per-user (`users/{uid}/...`) write is actually
    /// permitted for this path. Set by the catalog endpoint by AND-ing the
    /// declared scope with the server's allow/deny gate, so the editor never
    /// offers a per-user option the API would reject. Defaults to the declared
    /// scope until the endpoint refines it.
    pub per_user_allowed: bool,
    pub required: bool,
    pub description: String,
    /// The value spans lines (a whole file rather than a token), so the editor
    /// offers a text area instead of a single password field.
    #[serde(default)]
    pub multiline: bool,
}

/// Build a unified skills list from built-in catalog + filesystem scan, leaving out the external
/// skill directories `caller` may not use.
pub fn global_skills_list(
    skills_dir: &std::path::Path,
    caller: &crate::skill_access::SkillCaller,
) -> Vec<SkillInfo> {
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for meta in builtin_skill_catalog() {
        let dir_name = meta.dir_name();
        let has_config = skills_dir.join(dir_name).join("config.toml").exists();
        seen.insert(dir_name.to_string());
        skills.push(SkillInfo {
            name: meta.name.to_string(),
            dir_name: dir_name.to_string(),
            description: meta.description.to_string(),
            skill_type: "builtin".to_string(),
            has_config,
            supports_config: meta.supports_config,
            editable: false,
            required_credentials: meta
                .required_credentials
                .iter()
                .map(|c| CredentialInfo {
                    vault_path: c.vault_path.to_string(),
                    description: c.description.to_string(),
                    required: c.required,
                })
                .collect(),
            setup_hint: if meta.setup_hint.is_empty() {
                None
            } else {
                Some(meta.setup_hint.to_string())
            },
            workspace_vault_prefix: if meta.workspace_vault_prefix.is_empty() {
                None
            } else {
                Some(meta.workspace_vault_prefix.to_string())
            },
            workspace_credentials: meta
                .workspace_credentials
                .iter()
                .map(|wc| WorkspaceCredentialTemplate {
                    key: wc.key.to_string(),
                    description: wc.description.to_string(),
                    required: wc.required,
                })
                .collect(),
        });
    }

    // A live scan rather than the shared snapshot: it also lists config-only and unparseable
    // skill directories, so an operator can see and fix them.
    for scan_dir in &crate::external_skills::skill_roots(skills_dir) {
        let entries = match std::fs::read_dir(scan_dir) {
            Ok(e) => e,
            Err(e) => {
                if scan_dir == skills_dir {
                    warn!(path = %scan_dir.display(), error = %e, "Failed to read skills directory");
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if seen.contains(&dir_name) {
                continue;
            }
            let skill_md_path = path.join("SKILL.md");
            let has_config = path.join("config.toml").exists()
                || skills_dir.join(&dir_name).join("config.toml").exists();
            let has_skill_md = skill_md_path.exists();

            if !has_skill_md && !has_config {
                continue;
            }
            seen.insert(dir_name.clone());
            if !crate::external_skills::may_read_instructions(caller, &path) {
                continue;
            }

            let (description, required_credentials) = if has_skill_md {
                match std::fs::read_to_string(&skill_md_path).and_then(|content| {
                    workforce_skills::skill_file::SkillFile::parse(&content)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                }) {
                    Ok(skill_file) => {
                        let creds = skill_file
                            .meta
                            .requires
                            .vault
                            .into_iter()
                            .map(|v| CredentialInfo {
                                vault_path: v.path,
                                description: v.description,
                                required: v.required,
                            })
                            .collect();
                        (skill_file.meta.description, creds)
                    }
                    Err(e) => {
                        warn!(skill = %dir_name, error = %e, "Failed to parse SKILL.md");
                        (String::new(), Vec::new())
                    }
                }
            } else {
                (String::new(), Vec::new())
            };

            skills.push(SkillInfo {
                name: dir_name.clone(),
                dir_name,
                description,
                skill_type: "external".to_string(),
                has_config,
                supports_config: false,
                editable: true,
                required_credentials,
                setup_hint: None,
                workspace_vault_prefix: None,
                workspace_credentials: Vec::new(),
            });
        }
    }

    skills
}

/// Return metadata for all known built-in skills.
pub fn builtin_skill_catalog() -> Vec<BuiltinSkillMeta> {
    vec![
        BuiltinSkillMeta {
            name: "github",
            description: "Interact with GitHub repositories, issues, pull requests, and files",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/github/token",
                    description: "GitHub Personal Access Token (PAT)",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/github/app_id",
                    description: "GitHub App ID (or client ID) of the reviewer App; with the private key, pull-request approvals are submitted as the App",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/github/app_private_key",
                    description: "The reviewer App's RSA private key, PEM as GitHub generated it",
                    required: false,
                },
            ],
            tool_prefix: "github",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own PAT. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the PAT is \
shared across every user of this agent.\n\
\n\
WARNING: If GitHub is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-org pattern below to add new orgs alongside existing ones.\n\
\n\
Recommended (supports multiple orgs):\n\
  vault.set('skills/github/workspaces/<org>/token', '<pat>') for each org\n\
  vault.set('skills/github/default_workspace', '<default-org>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single org only:\n\
  vault.set('skills/github/token', '<pat>')\n\
\n\
Use fine-grained PATs scoped per org for multi-org mode.\n\
The 'owner' parameter on tools auto-selects the right token when it matches a workspace name.\n\
After adding or changing credentials, call system.enable_skill('github') to reload.",
            workspace_vault_prefix: "skills/github/workspaces/",
            workspace_credentials: &[WorkspaceCredentialMeta {
                key: "token",
                description: "GitHub Personal Access Token (PAT)",
                required: true,
                addable: true,
            }],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "google",
            description:
                "Google Workspace — Gmail, Calendar, Drive, Sheets, Docs, Tasks, Chat, Slides, Contacts",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/google/workspaces/default/refresh_token",
                    description:
                        "Per-user OAuth refresh token (created via google.setup action='login')",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/google/oauth_client/client_id",
                    description: "OAuth client ID (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/google/oauth_client/client_secret",
                    description: "OAuth client secret (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/google/oauth_client/redirect_uri",
                    description: "OAuth redirect URI (web-application clients only)",
                    required: false,
                },
            ],
            tool_prefix: "google",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Oauth,
            setup_hint: "\
Zero-to-working runbook for the Google skill. The agent should drive \
step 2 entirely from chat; step 1 is a one-time admin task per \
organization.\n\
\n\
━━━ STEP 1 — Create an OAuth client (one-time, per organization) ━━━\n\
\n\
This step needs a human with access to Google Cloud Console. It only \
has to be done once for the whole organization; all users reuse the \
same client.\n\
\n\
1. Go to https://console.cloud.google.com/apis/credentials and select \
   or create a project.\n\
2. Configure the OAuth consent screen:\n\
   - User type: 'Internal' if every user is on the same Google Workspace \
     domain (e.g. all @harbor.example). Internal apps skip Google's \
     verification review entirely — any scope, any number of users, no \
     publishing step.\n\
   - User type: 'External' otherwise. While Publishing status is \
     'Testing' (the default after creation), only accounts on the \
     'Test users' allowlist can authorize. Every hub user must be \
     added there — non-test-users hit `unauthorized_client` at token \
     exchange after appearing to consent. Add each user under \
     OAuth consent screen → Test users before they try to log in.\n\
3. Create credentials → OAuth client ID → Application type: 'Desktop \
   app' (recommended; uses the device flow), or 'Web application' \
   (uses the hub-hosted web-redirect flow, requires a public HTTPS \
   hub host with Authorized redirect URI \
   https://<your-hub-host>/api/auth/google/callback). Name it \
   'Workforce' (or whatever). Copy the Client ID and Client Secret.\n\
4. Enable the APIs the skill talks to: APIs & Services → Library → \
   enable Gmail API, Google Calendar API, Google Drive API, Google \
   Sheets API, Google Docs API, Google Tasks API, Google Chat API, \
   Google Slides API, and People API.\n\
5. Store the client credentials in the hub's GLOBAL vault — these are \
   organization-wide and shared by every user. They MUST be set from \
   the hub host (the agent-callable `vault.set` tool writes to a \
   per-agent vault that the OAuth callback handler cannot reach, so \
   setting them through an agent silently creates a shadowing trap):\n\
     workforce vault set skills/google/oauth_client/client_id     '<CLIENT_ID>.apps.googleusercontent.com'\n\
     workforce vault set skills/google/oauth_client/client_secret '<CLIENT_SECRET>'\n\
   (Web application client only:)\n\
     workforce vault set skills/google/oauth_client/redirect_uri \\\n\
       'https://<your-hub-host>/api/auth/google/callback'\n\
\n\
━━━ STEP 2 — Connect a user's Google account (per user, in chat) ━━━\n\
\n\
Drive this entirely from the `google.setup` agent tool. Do NOT tell \
the user to run any CLI command.\n\
\n\
a. Call `google.setup(action='login', no_browser=true, workspace='<name>')`.\n\
   - Use workspace='default' for single-account setups, or a distinct \
     workspace name per user/account otherwise (e.g. the email's \
     local-part).\n\
   - This returns immediately with `user_code`, `verification_url`, and \
     `expires_in_seconds`. It does NOT block.\n\
b. Show BOTH `user_code` and `verification_url` to the user verbatim. \
   Ask them to open the URL on any device (phone, laptop, work \
   computer), sign into the Google account they want to connect, and \
   enter the code. Typical time: 30–60 seconds.\n\
c. Call `google.setup(action='login_poll', workspace='<name>')`.\n\
   - Possible results:\n\
     • status='authorized' → they're done. Confirm with the user and \
       move on.\n\
     • status='pending' → they haven't finished. Ask if they need help; \
       wait a few seconds; call login_poll again. Repeat until \
       authorized, expired, or denied.\n\
     • status='expired' → code timed out; start over with action='login'.\n\
     • status='denied' → they clicked Cancel; check if they meant to.\n\
d. (Optional) Verify with `google.setup(action='status', workspace='<name>')`. \
   `authenticated: true` means every google.* tool can now use that \
   workspace via the `account` parameter.\n\
\n\
━━━ Multi-account tips ━━━\n\
\n\
• Set a default so tools without an `account` parameter pick the right one:\n\
    vault.set(scope='agent_specific', path='skills/google/default_workspace', value='<name>')\n\
  (shared by every user of this agent — an admin sets it)\n\
• List connected accounts: `google.setup(action='list')`.\n\
• Disconnect: `google.setup(action='logout', workspace='<name>')`.\n\
\n\
━━━ CLI alternative (only for developers onboarding themselves) ━━━\n\
\n\
The CLI `workforce google login [--workspace NAME] [--no-browser]` \
performs the same OAuth flow and is useful for local development. \
Regular users should never need it — the agent tool is the supported \
path.",
            workspace_vault_prefix: "skills/google/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "refresh_token",
                    description: "OAuth refresh token (auto-created via google.setup action='login')",
                    required: true,
                    addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "access_token",
                    description: "Short-lived OAuth access token (auto-managed by google.setup)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "expires_at",
                    description: "Access-token expiry timestamp (auto-managed by google.setup)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "scopes",
                    description: "Granted OAuth scopes (auto-managed by google.setup)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "email",
                    description: "Connected account email (auto-managed by google.setup)",
                    required: false,
                    addable: false,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "linear",
            description: "Interact with Linear issues, comments, workflow states, and teams",
            required_credentials: &[CredentialMeta {
                vault_path: "skills/linear/api_key",
                description: "Linear API Key",
                required: true,
            }],
            tool_prefix: "linear",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own API key. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the key is \
shared across every user of this agent.\n\
\n\
WARNING: If Linear is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-workspace pattern below to add new workspaces alongside existing ones.\n\
\n\
Recommended (supports multiple workspaces):\n\
  vault.set('skills/linear/workspaces/<name>/api_key', '<key>') for each workspace\n\
  vault.set('skills/linear/workspaces/<name>/team_keys', '<PREFIX>') — team key prefixes (e.g. 'DW' or 'STA,STAR')\n\
  vault.set('skills/linear/default_workspace', '<default-name>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single workspace only:\n\
  vault.set('skills/linear/api_key', '<key>')\n\
\n\
Each workspace maps to a separate Linear account/org with its own API key.\n\
Team key prefixes enable auto-detection: 'OPS-243' routes to the workspace owning team key 'STA'.\n\
After adding or changing credentials, call system.enable_skill('linear') to reload.",
            workspace_vault_prefix: "skills/linear/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "api_key",
                    description: "Linear API Key",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "team_keys",
                    description: "Team key prefixes (e.g. DW,STA)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "jira",
            description: "Interact with Jira Cloud issues, comments, transitions, boards, and sprints",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/jira/email",
                    description: "Atlassian account email",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/jira/api_token",
                    description: "Jira Cloud API token",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/jira/site",
                    description: "Atlassian site URL (https://<tenant>.atlassian.net)",
                    required: true,
                },
            ],
            tool_prefix: "jira",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own API token. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the token is \
shared across every user of this agent.\n\
\n\
WARNING: If Jira is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-tenant pattern below to add new tenants alongside existing ones.\n\
\n\
Recommended (supports multiple tenants):\n\
  vault.set('skills/jira/workspaces/<name>/email', '<you@example.com>')\n\
  vault.set('skills/jira/workspaces/<name>/api_token', '<token>')\n\
  vault.set('skills/jira/workspaces/<name>/site', 'https://<tenant>.atlassian.net')\n\
  vault.set('skills/jira/workspaces/<name>/project_keys', 'ENG,INFRA')  -- enables auto-routing\n\
  vault.set('skills/jira/default_workspace', '<default-name>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single tenant only:\n\
  vault.set('skills/jira/email', '<you@example.com>')\n\
  vault.set('skills/jira/api_token', '<token>')\n\
  vault.set('skills/jira/site', 'https://<tenant>.atlassian.net')\n\
\n\
Generate API tokens at https://id.atlassian.com/manage-profile/security/api-tokens\n\
Project-key prefixes enable auto-detection: 'ENG-123' routes to the workspace owning 'ENG'.\n\
After adding or changing credentials, call system.enable_skill('jira') to reload.",
            workspace_vault_prefix: "skills/jira/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "email",
                    description: "Atlassian account email",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "api_token",
                    description: "Jira Cloud API token",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "site",
                    description: "Tenant URL (https://<tenant>.atlassian.net)",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "project_keys",
                    description: "Comma-separated project key prefixes (e.g. ENG,INFRA)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "datadog",
            description: "Query Datadog metrics/logs and manage monitors, dashboards, downtimes, incidents, SLOs, and hosts",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/datadog/api_key",
                    description: "Datadog API key (DD-API-KEY header)",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/datadog/app_key",
                    description: "Datadog Application key (DD-APPLICATION-KEY header)",
                    required: true,
                },
            ],
            tool_prefix: "datadog",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own keys. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the keys are \
shared across every user of this agent.\n\
\n\
WARNING: If Datadog is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-account pattern below to add new accounts alongside existing ones.\n\
\n\
Recommended (supports multiple accounts):\n\
  vault.set('skills/datadog/accounts/<name>/api_key', '<api-key>')\n\
  vault.set('skills/datadog/accounts/<name>/app_key', '<app-key>')\n\
  vault.set('skills/datadog/accounts/<name>/site',    'us1')   -- us1|us3|us5|eu1|ap1\n\
  vault.set('skills/datadog/default_account', '<default-name>', scope='agent_specific')  -- only with several accounts\n\
\n\
Single account only:\n\
  vault.set('skills/datadog/api_key', '<api-key>')\n\
  vault.set('skills/datadog/app_key', '<app-key>')\n\
  vault.set('skills/datadog/site',    'us1')   -- optional, default us1\n\
\n\
Generate keys at:\n\
  https://app.datadoghq.com/organization-settings/api-keys\n\
  https://app.datadoghq.com/personal-settings/application-keys\n\
\n\
Each account is bound to one Datadog region (site).\n\
Write/destructive tools (monitor create/update/delete/mute, host mute, downtime create/cancel,\n\
event post) default to require_approval. Read tools default to allow.\n\
After adding or changing credentials, call system.enable_skill('datadog') to reload.",
            workspace_vault_prefix: "skills/datadog/accounts/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "api_key",
                    description: "Datadog API key",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "app_key",
                    description: "Datadog Application key",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "site",
                    description: "Datadog site key (us1|us3|us5|eu1|ap1)",
                    required: false,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "base_url",
                    description: "Optional base URL override (FedRAMP, custom domain)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "metabase",
            description: "Query Metabase cards/dashboards, run native SQL, manage collections across multiple Metabase instances",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/metabase/api_key",
                    description: "Metabase API key (X-API-KEY header, v0.49+)",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/metabase/site_url",
                    description: "Metabase site URL (e.g. https://mb.example.com)",
                    required: true,
                },
            ],
            tool_prefix: "metabase",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own API key. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the key is \
shared across every user of this agent.\n\
\n\
WARNING: If Metabase is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-instance pattern below to add new instances alongside existing ones.\n\
\n\
Recommended (supports multiple instances):\n\
  vault.set('skills/metabase/instances/<name>/api_key',  '<mb_...>')\n\
  vault.set('skills/metabase/instances/<name>/site_url', 'https://mb.example.com')\n\
  vault.set('skills/metabase/default_instance', '<default-name>', scope='agent_specific')  -- only with several instances\n\
\n\
Single instance only:\n\
  vault.set('skills/metabase/api_key',  '<mb_...>')\n\
  vault.set('skills/metabase/site_url', 'https://mb.example.com')\n\
\n\
Generate an API key at: /admin/settings/authentication/api-keys (Metabase v0.49+).\n\
\n\
card_query and dataset_query_native are row-capped (hard ceiling 2000). Set\n\
skills/metabase/instances/<name>/max_query_rows to override per instance.\n\
After adding or changing credentials, call system.enable_skill('metabase') to reload.",
            workspace_vault_prefix: "skills/metabase/instances/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "api_key",
                    description: "Metabase API key",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "site_url",
                    description: "Metabase site URL",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "max_query_rows",
                    description: "Optional per-instance row cap (default 2000, max 2000)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "xero",
            description: "Query and act on Xero accounting data (P&L, balance sheet, invoices, contacts, manual journals) across one or more organisations via Custom Connections",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/xero/client_id",
                    description: "Xero custom-connection client id",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/xero/client_secret",
                    description: "Xero custom-connection client secret",
                    required: true,
                },
            ],
            tool_prefix: "xero",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
Credentials come from a Xero Custom Connection (client-credentials grant), one \
per organisation. Create each at https://developer.xero.com/app/manage and \
subscribe it to the paid custom-connection plan.\n\
\n\
Recommended (supports multiple entities, e.g. one per company):\n\
  vault.set('skills/xero/instances/<name>/client_id',     '<client-id>')\n\
  vault.set('skills/xero/instances/<name>/client_secret', '<client-secret>')\n\
  vault.set('skills/xero/default_entity', '<default-name>', scope='agent_specific')  -- only with several entities\n\
  -- tenant_id is discovered automatically; set it only to pin an org:\n\
  vault.set('skills/xero/instances/<name>/tenant_id', '<tenant-id>')\n\
\n\
Single entity only:\n\
  vault.set('skills/xero/client_id',     '<client-id>')\n\
  vault.set('skills/xero/client_secret', '<client-secret>')\n\
\n\
Read tools (reports, lists) default to allow. Mutating tools (invoice/contact \
create/update, manual journal, bill approve, payment, invoice email) default to \
require_approval. Report tools (trial balance, aged receivables, bank summary) \
need the accounting.reports.read scope on the connection; bank transactions need \
accounting.transactions.read; write tools need the matching write scopes.\n\
After adding or changing credentials, call system.enable_skill('xero') to reload.",
            workspace_vault_prefix: "skills/xero/instances/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "client_id",
                    description: "Xero custom-connection client id",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "client_secret",
                    description: "Xero custom-connection client secret",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "tenant_id",
                    description: "Optional fixed tenant id (auto-discovered if omitted)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "spotify",
            description: "Control Spotify playback — play, pause, skip, volume, and what is playing — preferring a named speaker so a spoken request reaches the room it was spoken in",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/spotify/account/me/refresh_token",
                    description: "Per-user OAuth refresh token (created via spotify.setup action='login')",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/spotify/oauth_client/client_id",
                    description: "OAuth app client id (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/spotify/oauth_client/client_secret",
                    description: "OAuth app client secret (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/spotify/oauth_client/redirect_uri",
                    description: "OAuth redirect URI — https://<hub>/api/auth/spotify/callback",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/spotify/oauth_client/device",
                    description: "Speaker to prefer for playback (default 'Allan', the voice satellite)",
                    required: false,
                },
            ],
            tool_prefix: "spotify",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Oauth,
            setup_hint: "\
Spotify uses 3-legged OAuth 2.0, and controlling playback requires a Spotify \
PREMIUM account — Spotify refuses these calls on free accounts whatever the \
permissions say.\n\
\n\
━━━ STEP 1 — Register an app (one-time, per organization) ━━━\n\
\n\
1. At https://developer.spotify.com/dashboard, create an app and copy its \
   Client ID and Client Secret. Add this exact Redirect URI to the app:\n\
     https://<your-hub-host>/api/auth/spotify/callback\n\
   It must match character for character — a mismatch is the most common \
   failure here and Spotify reports it only as INVALID_CLIENT.\n\
2. Store the credentials in the hub's GLOBAL vault from the hub host (an \
   agent-set vault is per-agent and the callback cannot read it):\n\
     workforce vault set skills/spotify/oauth_client/client_id     '<CLIENT_ID>'\n\
     workforce vault set skills/spotify/oauth_client/client_secret '<CLIENT_SECRET>'\n\
     workforce vault set skills/spotify/oauth_client/redirect_uri  'https://<your-hub-host>/api/auth/spotify/callback'\n\
   Optionally name the speaker to prefer (defaults to 'Allan'):\n\
     workforce vault set skills/spotify/oauth_client/device 'Allan'\n\
\n\
━━━ STEP 2 — Connect the account (per user, in chat) ━━━\n\
\n\
a. Call spotify.setup(action='login'). It returns an authorization_url; relay \
   it to the user verbatim.\n\
b. The user opens it, signs into Spotify and approves; the hub captures the \
   result automatically.\n\
c. Call spotify.setup(action='login_poll') until status='authorized'.\n\
\n\
All playback tools default to allow rather than require_approval: the surface \
they exist for is a speaker with no screen, where an approval prompt cannot be \
seen or answered. Disconnect with spotify.setup(action='logout'). After the \
admin step, call system.enable_skill('spotify') to reload.",
            workspace_vault_prefix: "",
            workspace_credentials: &[],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "freeagent",
            description: "Query and act on FreeAgent accounting data (invoices, bills, contacts, estimates, expenses, bank transactions, projects, timeslips, P&L) for connected companies",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/freeagent/workspaces/default/refresh_token",
                    description: "Per-user OAuth refresh token (created via freeagent.setup action='login')",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/freeagent/oauth_client/client_id",
                    description: "OAuth app client id (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/freeagent/oauth_client/client_secret",
                    description: "OAuth app client secret (organization-wide, set in the global vault)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/freeagent/oauth_client/redirect_uri",
                    description: "OAuth redirect URI — https://<hub>/api/auth/freeagent/callback",
                    required: false,
                },
            ],
            tool_prefix: "freeagent",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Oauth,
            setup_hint: "\
FreeAgent uses 3-legged OAuth 2.0. Onboarding is two parts: a one-time admin \
step per organization, then a per-user connect driven from chat.\n\
\n\
━━━ STEP 1 — Register an OAuth app (one-time, per organization) ━━━\n\
\n\
1. At https://dev.freeagent.com, create an app and copy its Client ID and \
   Client Secret. Set the OAuth redirect URI to \
   https://<your-hub-host>/api/auth/freeagent/callback.\n\
2. Store the credentials in the hub's GLOBAL vault from the hub host (an \
   agent-set vault is per-agent and the callback cannot read it):\n\
     workforce vault set skills/freeagent/oauth_client/client_id     '<CLIENT_ID>'\n\
     workforce vault set skills/freeagent/oauth_client/client_secret '<CLIENT_SECRET>'\n\
     workforce vault set skills/freeagent/oauth_client/redirect_uri  'https://<your-hub-host>/api/auth/freeagent/callback'\n\
   (For the FreeAgent sandbox, also set skills/freeagent/oauth_client/sandbox 'true'.)\n\
\n\
━━━ STEP 2 — Connect a company (per user, in chat) ━━━\n\
\n\
a. Call freeagent.setup(action='login', workspace='<name>'). It returns an \
   authorization_url; relay it to the user verbatim (the 'next_step' field has \
   the exact wording). Use workspace='default' for a single company.\n\
b. The user opens the URL, signs into FreeAgent, and approves; the hub captures \
   the result automatically.\n\
c. Call freeagent.setup(action='login_poll', workspace='<name>') until \
   status='authorized'.\n\
\n\
Read tools default to allow; mutating tools (contact/invoice/timeslip/expense \
create, contact update) default to require_approval. List/disconnect via \
freeagent.setup(action='list'|'logout'). After the admin step, call \
system.enable_skill('freeagent') to reload.",
            workspace_vault_prefix: "skills/freeagent/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "refresh_token",
                    description: "OAuth refresh token (auto-created via freeagent.setup)",
                    required: true,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "access_token",
                    description: "Short-lived OAuth access token (auto-managed by freeagent.setup)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "expires_at",
                    description: "Access-token expiry timestamp (auto-managed)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "scopes",
                    description: "Granted OAuth scopes (auto-managed)",
                    required: false,
                    addable: false,
                },
                WorkspaceCredentialMeta {
                    key: "company",
                    description: "Connected company name (auto-managed)",
                    required: false,
                    addable: false,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "postmark",
            description: "Send transactional email and inspect delivery status, bounces, stats, templates, streams, and suppressions via the Postmark REST API across multiple servers",
            required_credentials: &[CredentialMeta {
                vault_path: "skills/postmark/server_token",
                description: "Postmark Server API token (X-Postmark-Server-Token header)",
                required: true,
            }],
            tool_prefix: "postmark",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own tokens. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the tokens are \
shared across every user of this agent.\n\
\n\
WARNING: If Postmark is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-workspace pattern below to add new servers alongside existing ones.\n\
\n\
Recommended (supports multiple Postmark servers):\n\
  vault.set('skills/postmark/workspaces/<name>/server_token',  '<pm_...>')\n\
  vault.set('skills/postmark/workspaces/<name>/account_token', '<pm_...>')   -- optional\n\
  vault.set('skills/postmark/workspaces/<name>/base_url',      'https://api.postmarkapp.com') -- optional\n\
  vault.set('skills/postmark/default_workspace', '<default-name>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single workspace only:\n\
  vault.set('skills/postmark/server_token', '<pm_...>')\n\
  vault.set('skills/postmark/account_token', '<pm_...>')   -- optional\n\
\n\
Postmark uses two token scopes:\n\
  - Server token (required): per-server operations — send, messages, stats,\n\
    bounces, templates, streams, suppressions.\n\
  - Account token (optional): account-wide — servers_list, domains_list,\n\
    domain_get, and postmark.api with token_type=account. Without it, those\n\
    four tools return a clear runtime error but every other tool still works.\n\
\n\
Generate tokens in Postmark:\n\
  Server token  → Servers > <server> > API Tokens\n\
  Account token → Account > API Tokens\n\
\n\
Write tools (email_send, email_send_batch, email_send_template,\n\
bounce_activate, suppressions_add, suppressions_delete, postmark.api) default\n\
to require_approval. Read tools default to allow.\n\
After adding or changing credentials, call system.enable_skill('postmark') to reload.",
            workspace_vault_prefix: "skills/postmark/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "server_token",
                    description: "Postmark Server API token (required)",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "account_token",
                    description:
                        "Postmark Account API token (optional; enables servers_list, domains_list, domain_get)",
                    required: false,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "base_url",
                    description:
                        "Optional Postmark base URL override (default https://api.postmarkapp.com)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "sendgrid",
            description: "Send transactional email and inspect delivery, stats, templates, sender identities and suppressions via the Twilio SendGrid v3 API across multiple accounts",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/sendgrid/api_key",
                    description: "SendGrid API key (Authorization: Bearer)",
                    required: true,
                },
                CredentialMeta {
                    vault_path: "skills/sendgrid/from_email",
                    description: "Default From address (must be a verified sender identity)",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/sendgrid/from_name",
                    description: "Display name for the default From address",
                    required: false,
                },
                CredentialMeta {
                    vault_path: "skills/sendgrid/base_url",
                    description: "Optional API base URL override (https://api.eu.sendgrid.com for EU accounts)",
                    required: false,
                },
            ],
            tool_prefix: "sendgrid",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own key. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the key is \
shared across every user of this agent.\n\
\n\
WARNING: If SendGrid is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-workspace pattern below to add new accounts alongside existing ones.\n\
\n\
Recommended (supports multiple SendGrid accounts or subusers):\n\
  vault.set('skills/sendgrid/workspaces/<name>/api_key',    '<SG....>')\n\
  vault.set('skills/sendgrid/workspaces/<name>/from_email', 'noreply@example.com')  -- optional default From\n\
  vault.set('skills/sendgrid/workspaces/<name>/from_name',  'Example')              -- optional\n\
  vault.set('skills/sendgrid/workspaces/<name>/base_url',   'https://api.eu.sendgrid.com') -- optional, EU residency\n\
  vault.set('skills/sendgrid/default_workspace', '<default-name>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single workspace only:\n\
  vault.set('skills/sendgrid/api_key',    '<SG....>')\n\
  vault.set('skills/sendgrid/from_email', 'noreply@example.com')  -- optional\n\
\n\
Create the key in SendGrid → Settings → API Keys. A restricted key with Mail Send,\n\
Template Engine (read), Stats (read), Suppressions and Sender Authentication (read)\n\
covers every typed tool. The From address must be a verified sender identity or on\n\
an authenticated domain, else sends fail with 403.\n\
\n\
Write tools (email_send, email_send_batch, suppression_delete, unsubscribes_add,\n\
sendgrid.api) default to require_approval. Read tools default to allow.\n\
After adding or changing credentials, call system.enable_skill('sendgrid') to reload.",
            workspace_vault_prefix: "skills/sendgrid/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "api_key",
                    description: "SendGrid API key (required)",
                    required: true,
                    addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "from_email",
                    description: "Default From address (optional; must be a verified sender)",
                    required: false,
                    addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "from_name",
                    description: "Default From display name (optional)",
                    required: false,
                    addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "base_url",
                    description: "Optional API base URL override (https://api.eu.sendgrid.com for EU accounts)",
                    required: false,
                    addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "hubspot",
            description: "Manage HubSpot CRM contacts, companies, deals, tickets and custom objects; notes, tasks, calls, emails and meetings; associations, pipelines, owners and properties; plus any other endpoint through hubspot.api — across multiple portals",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/hubspot/access_token",
                    description: "HubSpot Private App access token (Bearer auth)",
                    required: true,
                },
            ],
            tool_prefix: "hubspot",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own access token. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the token is \
shared across every user of this agent.\n\
\n\
WARNING: If HubSpot is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-portal pattern below to add new portals alongside existing ones.\n\
\n\
Recommended (supports multiple portals):\n\
  vault.set('skills/hubspot/portals/<name>/access_token', '<pat-...>')\n\
  vault.set('skills/hubspot/default_portal', '<default-name>', scope='agent_specific')  -- only with several portals\n\
\n\
Single portal only:\n\
  vault.set('skills/hubspot/access_token', '<pat-...>')\n\
\n\
Generate a Private App token at: Settings > Integrations > Private Apps in HubSpot.\n\
Required scopes: crm.objects.contacts.read, crm.objects.contacts.write, \
crm.objects.companies.read, crm.objects.companies.write, \
crm.objects.deals.read, crm.objects.deals.write, \
tickets, crm.objects.owners.read; custom objects need crm.objects.custom.read, \
crm.objects.custom.write and crm.schemas.custom.read; notes, tasks, calls, \
emails and meetings on a record need that record type's write scope.\n\
\n\
List/search results are capped at 100.\n\
After adding or changing credentials, call system.enable_skill('hubspot') to reload.",
            workspace_vault_prefix: "skills/hubspot/portals/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "access_token",
                    description: "HubSpot Private App access token",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "max_results",
                    description: "Optional per-portal result cap (default 100, max 100)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "notion",
            description: "Manage Notion pages, databases, blocks, users, and comments across multiple workspaces",
            required_credentials: &[
                CredentialMeta {
                    vault_path: "skills/notion/token",
                    description: "Notion internal integration token (Bearer auth)",
                    required: true,
                },
            ],
            tool_prefix: "notion",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own integration token. Pass scope='agent_shared' \
on the vault.set calls below only when the user has explicitly said the token is \
shared across every user of this agent.\n\
\n\
WARNING: If Notion is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-workspace pattern below to add new workspaces alongside existing ones.\n\
\n\
Recommended (supports multiple workspaces):\n\
  vault.set('skills/notion/workspaces/<name>/token', '<ntn_...>')\n\
  vault.set('skills/notion/default_workspace', '<default-name>', scope='agent_specific')  -- only with several workspaces\n\
\n\
Single workspace only:\n\
  vault.set('skills/notion/token', '<ntn_...>')\n\
\n\
Create an internal integration at: https://www.notion.so/my-integrations\n\
Then share each page/database with the integration in Notion.\n\
\n\
Rate limit: 3 requests/second per integration.\n\
After adding or changing credentials, call system.enable_skill('notion') to reload.",
            workspace_vault_prefix: "skills/notion/workspaces/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "token",
                    description: "Notion internal integration token",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "max_results",
                    description: "Optional per-workspace result cap (default 100, max 100)",
                    required: false,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "browser",
            description:
                "Headless browser automation — navigate, screenshot, PDF, interact with elements, inspect network/console (Chromium via CDP)",
            required_credentials: &[],
            tool_prefix: "browser",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint:
                "No credentials required — uses the bundled Chromium binary. Skill spawns one Chromium subprocess per agent, lazily on first call.",
            workspace_vault_prefix: "",
            workspace_credentials: &[],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "obscura",
            description:
                "Headless browser automation via Obscura over stdio MCP — lower memory than the `browser` skill, but no screenshot or PDF. Choose for text-only scraping / form automation.",
            required_credentials: &[],
            tool_prefix: "obscura",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint:
                "No credentials required — uses the bundled `obscura` binary. Skill spawns one obscura subprocess per agent, lazily on first call.",
            workspace_vault_prefix: "",
            workspace_credentials: &[],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "email",
            description:
                "Multi-account email integration — read, search, send, and manage email via IMAP/SMTP",
            required_credentials: &[CredentialMeta {
                vault_path: "skills/email/accounts/*/username",
                description: "Email account username",
                required: true,
            }],
            tool_prefix: "email",
            default_scope: CredScope::Either,
            managed_by: CredManaged::Manual,
            setup_hint: "\
By default vault.set stores credentials in the active user's per-user namespace \
(users/{uid}/...) — every user must add their own mailbox credentials. Pass \
scope='agent_shared' on the vault.set calls below only when the user has \
explicitly said the mailbox is shared across every user of this agent.\n\
\n\
WARNING: If Email is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-account pattern below to add new accounts alongside existing ones.\n\
\n\
For each email account:\n\
  vault.set('skills/email/accounts/<name>/username', '<email>')\n\
  vault.set('skills/email/accounts/<name>/password', '<password>')\n\
  vault.set('skills/email/accounts/<name>/imap_host', '<host>')\n\
  vault.set('skills/email/accounts/<name>/smtp_host', '<host>')\n\
\n\
Optional (have defaults):\n\
  vault.set('skills/email/accounts/<name>/imap_port', '993')\n\
  vault.set('skills/email/accounts/<name>/smtp_port', '587')\n\
  vault.set('skills/email/accounts/<name>/tls', 'true')\n\
  vault.set('skills/email/accounts/<name>/display_name', 'My Name')\n\
\n\
After adding credentials, call system.enable_skill('email') to load.",
            workspace_vault_prefix: "skills/email/accounts/",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "username",
                    description: "Email address",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "password",
                    description: "Email password or app password",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "imap_host",
                    description: "IMAP server hostname",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "smtp_host",
                    description: "SMTP server hostname",
                    required: true,
                addable: true,
                },
            ],
            supports_config: false,
            default_config_template: "",
        },
        BuiltinSkillMeta {
            name: "slack",
            description: "Slack messaging — send, read, and manage messages across workspaces",
            required_credentials: &[CredentialMeta {
                vault_path: "slack.bot_token",
                description: "Slack Bot Token (xoxb-...)",
                required: true,
            }],
            tool_prefix: "slack",
            default_scope: CredScope::Shared,
            managed_by: CredManaged::Manual,
            setup_hint: "\
Slack workspace credentials are SHARED across every user of this agent — the \
bot installs the workspace, and the same token serves every user. You MUST pass \
scope='agent_shared' on every vault.set call below; the default per-user scope \
would scope the token to one user only and break Slack delivery for everyone else.\n\
\n\
WARNING: If Slack is already loaded, do NOT overwrite existing credentials.\n\
Use the multi-workspace pattern below to add new workspaces alongside existing ones.\n\
\n\
For each Slack workspace:\n\
  vault.set(scope='agent_shared', path='slack.<workspace_id>.bot_token',     value='<xoxb-...>')\n\
  vault.set(scope='agent_shared', path='slack.<workspace_id>.app_token',     value='<xapp-...>')\n\
  vault.set(scope='agent_shared', path='slack.<workspace_id>.signing_secret', value='<secret>')\n\
\n\
After adding or changing credentials, call system.enable_skill('slack') to reload.",
            workspace_vault_prefix: "slack.",
            workspace_credentials: &[
                WorkspaceCredentialMeta {
                    key: "bot_token",
                    description: "Slack Bot Token (xoxb-...)",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "app_token",
                    description: "Slack App-Level Token (xapp-...)",
                    required: true,
                addable: true,
                },
                WorkspaceCredentialMeta {
                    key: "signing_secret",
                    description: "Slack Signing Secret",
                    required: false,
                addable: true,
                },
            ],
            supports_config: true,
            default_config_template: SLACK_DEFAULT_CONFIG_TEMPLATE,
        },
    ]
}

/// Look up the default `config.toml` template for a builtin skill by its
/// directory name (e.g., "slack-integration" → Slack template). Returns
/// `None` for unknown skills or builtins without a template.
pub fn builtin_config_template(dir_name: &str) -> Option<&'static str> {
    builtin_skill_catalog()
        .into_iter()
        .find(|meta| meta.dir_name() == dir_name)
        .map(|meta| meta.default_config_template)
        .filter(|tpl| !tpl.is_empty())
}

/// Derive the variable name + label for a workspace prefix's `{var}` segment
/// from the prefix's trailing path segment (e.g. ".../accounts/" → "account").
fn workspace_variable(prefix: &str) -> (&'static str, &'static str) {
    let seg = prefix
        .trim_end_matches(['/', '.'])
        .rsplit(['/', '.'])
        .next()
        .unwrap_or("name");
    match seg {
        "workspaces" => ("workspace", "Workspace name"),
        "accounts" => ("account", "Account name"),
        "instances" => ("instance", "Instance name"),
        "portals" => ("portal", "Portal name"),
        "slack" => ("workspace", "Workspace ID"),
        _ => ("name", "Name"),
    }
}

/// Non-skill credential families that follow a known path convention but are
/// not skills (LLM provider accounts, Claude Code OAuth). Surfaced in the
/// vault catalog alongside skills so the dropdown covers "any tool/account".
fn credential_families() -> Vec<CatalogEntry> {
    // Speech (TTS) voices: plain values, per-provider (the API keys are
    // `credentials::Modality::Speech`). Voices are PER-USER only: the shared
    // voice lives in `[llm.speech].voice` config, and nothing reads a bare
    // `speech/*/voice` vault path — offering a shared tier for it would be a
    // silently inert setting.
    let speech = [
        (
            "speech/fish_audio/voice",
            "Fish Audio — my voice",
            "Fish Audio voice model reference id (per user)",
            CredScope::PerUser,
        ),
        (
            "speech/elevenlabs/voice",
            "ElevenLabs — my voice",
            "ElevenLabs voice id (per user)",
            CredScope::PerUser,
        ),
        (
            "speech/openai/voice",
            "OpenAI speech — my voice",
            "OpenAI voice name, e.g. alloy or nova (per user)",
            CredScope::PerUser,
        ),
        (
            "speech/soniox/voice",
            "Soniox — my voice",
            "Soniox voice name, e.g. Maya or Adrian (per user)",
            CredScope::PerUser,
        ),
        (
            "speech/builtin/voice",
            "Built-in speech — my voice",
            "Piper voice id already installed on the agent (per user)",
            CredScope::PerUser,
        ),
    ]
    .into_iter()
    .map(|(path, label, description, scope)| CatalogEntry {
        id: path.to_string(),
        skill: None,
        group: "speech".to_string(),
        label: label.to_string(),
        template: path.to_string(),
        variables: Vec::new(),
        source: "builtin",
        addable: true,
        scope: scope.as_str(),
        managed_by: CredManaged::Manual.as_str(),
        per_user_allowed: scope.allows_per_user(),
        required: false,
        description: description.to_string(),
        multiline: false,
    });

    let mut entries: Vec<CatalogEntry> = speech.collect();
    entries.extend(vec![
        CatalogEntry {
            id: "llm/accounts/{name}".to_string(),
            skill: None,
            group: "llm".to_string(),
            label: "LLM provider account".to_string(),
            template: "llm/accounts/{name}".to_string(),
            variables: vec![CatalogVariable {
                name: "name".to_string(),
                label: "Account name".to_string(),
                placeholder: "change-me".to_string(),
                example: Some("gemini".to_string()),
            }],
            source: "builtin",
            addable: true,
            scope: CredScope::Shared.as_str(),
            managed_by: CredManaged::Manual.as_str(),
            per_user_allowed: CredScope::Shared.allows_per_user(),
            required: false,
            description: "LLM provider account credentials".to_string(),
            multiline: false,
        },
        CatalogEntry {
            id: "claude_code/oauth_token".to_string(),
            skill: None,
            group: "claude_code".to_string(),
            label: "Claude Code OAuth token".to_string(),
            template: "claude_code/oauth_token".to_string(),
            variables: vec![],
            source: "builtin",
            addable: true,
            scope: CredScope::PerUser.as_str(),
            managed_by: CredManaged::Oauth.as_str(),
            per_user_allowed: CredScope::PerUser.allows_per_user(),
            required: false,
            description: "Claude Code / Max OAuth token (per user)".to_string(),
            multiline: false,
        },
        CatalogEntry {
            id: "codex/auth_json".to_string(),
            skill: None,
            group: "codex".to_string(),
            label: "OpenAI Codex login".to_string(),
            template: "codex/auth_json".to_string(),
            variables: vec![],
            source: "builtin",
            addable: true,
            scope: CredScope::Either.as_str(),
            managed_by: CredManaged::Manual.as_str(),
            per_user_allowed: CredScope::Either.allows_per_user(),
            required: false,
            description: "auth.json as `codex login --device-auth` wrote it, pasted whole; per user, or agent-specific for a login the whole agent shares".to_string(),
            multiline: true,
        },
        CatalogEntry {
            id: "skills/catalog/{name}/token".to_string(),
            skill: None,
            group: "catalog".to_string(),
            label: "Skill-catalog read token".to_string(),
            template: "skills/catalog/{name}/token".to_string(),
            variables: vec![CatalogVariable {
                name: "name".to_string(),
                label: "Catalog name".to_string(),
                placeholder: "change-me".to_string(),
                example: Some("harbor".to_string()),
            }],
            source: "builtin",
            addable: true,
            scope: CredScope::Shared.as_str(),
            managed_by: CredManaged::Manual.as_str(),
            per_user_allowed: CredScope::Shared.allows_per_user(),
            required: false,
            description:
                "GitHub token for reading a skill catalog's private repo (org credential — \
                 separate from users' own github tokens; the catalog name matches \
                 [[skills.catalogs]])"
                    .to_string(),
            multiline: false,
        },
    ]);
    entries.extend(crate::credentials::catalog_entries());
    entries
}

/// Per-skill "default <thing>" selector pointers (`skills/<skill>/default_<x>`)
/// that pick the active workspace when a tool omits it. Recognised + addable so
/// the pointer is never badged unrecognised. Suffixes are irregular (xero uses
/// `default_entity` despite an `instances/` prefix), so they are listed
/// explicitly rather than derived from the workspace prefix.
const DEFAULT_SELECTORS: &[(&str, &str, &str)] = &[
    (
        "github",
        "skills/github/default_workspace",
        "Default workspace name",
    ),
    (
        "google",
        "skills/google/default_workspace",
        "Default workspace name",
    ),
    (
        "linear",
        "skills/linear/default_workspace",
        "Default workspace name",
    ),
    (
        "jira",
        "skills/jira/default_workspace",
        "Default workspace name",
    ),
    (
        "datadog",
        "skills/datadog/default_account",
        "Default account name",
    ),
    (
        "metabase",
        "skills/metabase/default_instance",
        "Default instance name",
    ),
    ("xero", "skills/xero/default_entity", "Default entity name"),
    (
        "postmark",
        "skills/postmark/default_workspace",
        "Default workspace name",
    ),
    (
        "sendgrid",
        "skills/sendgrid/default_workspace",
        "Default workspace name",
    ),
    (
        "hubspot",
        "skills/hubspot/default_portal",
        "Default portal name",
    ),
    (
        "notion",
        "skills/notion/default_workspace",
        "Default workspace name",
    ),
    (
        "email",
        "skills/email/default_account",
        "Default account name",
    ),
];

fn default_selector_entries() -> impl Iterator<Item = CatalogEntry> {
    DEFAULT_SELECTORS
        .iter()
        .map(|&(skill, path, description)| CatalogEntry {
            id: path.to_string(),
            skill: Some(skill.to_string()),
            group: skill.to_string(),
            label: format!("{skill} — {description}"),
            template: path.to_string(),
            variables: Vec::new(),
            source: "builtin",
            addable: true,
            scope: CredScope::Shared.as_str(),
            managed_by: CredManaged::Manual.as_str(),
            per_user_allowed: CredScope::Shared.allows_per_user(),
            required: false,
            description: description.to_string(),
            multiline: false,
        })
}

pub fn is_default_selector(path: &str) -> bool {
    DEFAULT_SELECTORS.iter().any(|(_, p, _)| *p == path)
}

/// Produce a concrete path from a template by replacing a `{var}` segment with
/// a placeholder. Used to test a template against prefix-based allow/deny gates.
pub fn representative_path(template: &str) -> String {
    match (template.find('{'), template.find('}')) {
        (Some(open), Some(close)) if close > open => {
            format!("{}x{}", &template[..open], &template[close + 1..])
        }
        _ => template.to_string(),
    }
}

/// Build the flattened credential catalog the vault editor consumes: every
/// known credential path across all built-in skills plus non-skill families,
/// each tagged with scope + managed_by + any `{var}` segments.
pub fn credential_catalog() -> Vec<CatalogEntry> {
    let mut entries = Vec::new();

    for meta in builtin_skill_catalog() {
        let scope = meta.default_scope.as_str();
        let managed = meta.managed_by.as_str();
        let group = meta.name.to_string();

        // Single-instance (fixed) credential paths.
        for c in meta.required_credentials {
            // Skip illustrative wildcard paths (e.g. ".../accounts/*/username").
            if c.vault_path.contains('*') {
                continue;
            }
            entries.push(CatalogEntry {
                id: c.vault_path.to_string(),
                skill: Some(meta.name.to_string()),
                group: group.clone(),
                label: format!("{} — {}", meta.name, c.description),
                template: c.vault_path.to_string(),
                variables: Vec::new(),
                source: "builtin",
                addable: true,
                scope,
                managed_by: managed,
                per_user_allowed: meta.default_scope.allows_per_user(),
                required: c.required,
                description: c.description.to_string(),
                multiline: false,
            });
        }

        // Multi-instance (templated) per-workspace credential paths.
        if !meta.workspace_vault_prefix.is_empty() {
            let prefix = meta.workspace_vault_prefix;
            let dotted = prefix.ends_with('.');
            let (var_name, var_label) = workspace_variable(prefix);
            for wc in meta.workspace_credentials {
                let template = if dotted {
                    format!("{prefix}{{{var_name}}}.{}", wc.key)
                } else {
                    format!("{prefix}{{{var_name}}}/{}", wc.key)
                };
                entries.push(CatalogEntry {
                    id: template.clone(),
                    skill: Some(meta.name.to_string()),
                    group: group.clone(),
                    label: format!("{} — {} (per {var_name})", meta.name, wc.description),
                    template,
                    variables: vec![CatalogVariable {
                        name: var_name.to_string(),
                        label: var_label.to_string(),
                        placeholder: "change-me".to_string(),
                        example: None,
                    }],
                    source: "builtin",
                    addable: wc.addable,
                    scope,
                    managed_by: managed,
                    per_user_allowed: meta.default_scope.allows_per_user(),
                    required: wc.required,
                    description: wc.description.to_string(),
                    multiline: false,
                });
            }
        }
    }

    entries.extend(default_selector_entries());
    entries.extend(credential_families());
    entries.extend(crate::mcp_servers::presets::mcp_credential_catalog());
    entries
}

const SLACK_DEFAULT_CONFIG_TEMPLATE: &str = r##"# Slack Integration Configuration
#
# NOTE: bot_token and app_token in the [[workspaces]] block below are NOT
# required if you have already set them in the agent vault as
# `slack.<workspace>.bot_token` and `slack.<workspace>.app_token`. The vault
# values take precedence and are the preferred way to manage secrets.
# The empty strings here are just placeholders so the block is syntactically
# valid.
#
# Resolution precedence for response_mode / thread_isolation / activation_keywords:
#   per-channel override → per-workspace override → top-level default.
#
# IMPORTANT: top-level defaults MUST appear before the first `[[workspaces]]`
# header, otherwise TOML scopes them to the workspace block and they become
# per-workspace overrides instead of defaults.

# Top-level defaults — apply to any workspace that does not override them.

# Default response mode: "mention_only" | "all" | "quiet"
response_mode = "mention_only"

# Thread isolation: each thread gets its own session context
thread_isolation = true

# Blank line between paragraphs and sections, so messages are easier to read
paragraph_spacing = true

# Activate on these keywords (case-insensitive), even without @mention
# activation_keywords = ["help", "deploy"]

# Agent bindings: map workspace_id to agent_id
# [agent_bindings]
# default = "my-agent"

[[workspaces]]
id = "default"
bot_token = ""
app_token = ""
# channels = []  # empty = all channels
# signing_secret = ""  # only needed if NOT using socket mode

# Per-workspace overrides (take precedence over the top-level defaults above):
# response_mode = "all"
# thread_isolation = false
# paragraph_spacing = false
# activation_keywords = ["deploy"]

# Per-channel overrides (take precedence over per-workspace overrides):
# [channel_overrides.C01234ABCDE]
# system_prompt = "This is #dev — focused on development."
# response_mode = "all"
# display_name = "#dev"

# Per-channel scope filter (optional): classifies inbound messages against a
# topic and short-circuits off-topic ones with a canned response. Adding a
# [channel_overrides.X.scope] block enables it.
#
# Preset personas: triage | faq | qa | support | sales
# [channel_overrides.C_TRIAGE.scope]
# persona = "triage"
# mode = "reject"          # "reject" (default) | "warn"
# canned_response = "I'm the triage bot. Please file bug reports here."
# model = "claude-haiku-4-5"  # optional classifier model
#
# Or define a custom topic:
# [channel_overrides.C_API_DOCS.scope]
# custom_prompt = """
# On-topic: API usage questions, API bug reports, docs clarifications.
# Off-topic: general chat, unrelated tech questions, personal questions.
# """
"##;

// ─── Credential Status ───────────────────────────────────────────────────────

/// Status of a skill's credential requirements.
#[derive(Debug, Serialize)]
pub struct SkillCredentialStatus {
    pub name: String,
    pub description: String,
    pub credentials: Vec<CredentialStatus>,
    /// All required credentials are present in the vault.
    pub ready: bool,
    /// Setup instructions for the agent.
    pub setup_hint: String,
    /// Discovered workspace names from vault (for multi-workspace skills).
    pub configured_workspaces: Vec<String>,
}

/// Status of a single credential.
#[derive(Debug, Serialize)]
pub struct CredentialStatus {
    pub vault_path: String,
    pub description: String,
    pub configured: bool,
    pub required: bool,
}

/// Check which skills have credentials available in the vault.
///
/// Resolves through `LayeredUserVault` so per-user overlays
/// (`users/{uid}/...`) surface alongside org-wide entries when
/// `CURRENT_USER_ID` is in scope.
pub fn check_skill_credentials(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
) -> Vec<SkillCredentialStatus> {
    let store = layered_user_store(vault, global_vault);
    builtin_skill_catalog()
        .iter()
        .map(|meta| {
            let credentials: Vec<CredentialStatus> = meta
                .required_credentials
                .iter()
                .map(|cred| CredentialStatus {
                    vault_path: cred.vault_path.to_string(),
                    description: cred.description.to_string(),
                    configured: store.get(cred.vault_path).is_some(),
                    required: cred.required,
                })
                .collect();

            // Discover workspace keys if the skill supports multi-workspace.
            let configured_workspaces =
                discover_workspaces(store.as_ref(), meta.workspace_vault_prefix);

            // Ready if single-credential path is satisfied OR workspace keys exist.
            let single_ready = credentials
                .iter()
                .filter(|c| c.required)
                .all(|c| c.configured);
            let ready = single_ready || !configured_workspaces.is_empty();

            SkillCredentialStatus {
                name: meta.name.to_string(),
                description: meta.description.to_string(),
                credentials,
                ready,
                setup_hint: meta.setup_hint.to_string(),
                configured_workspaces,
            }
        })
        .collect()
}

/// Wrap the agent vault in a `LayeredUserVault` that prefers per-user overlays
/// from the hub's global vault (written by the My Settings UI) over shared
/// agent credentials. When `global_vault` is `None`, per-user lookups fall
/// through to `users/{uid}/...` entries in the agent vault itself.
fn layered_user_store(
    agent_vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
) -> Arc<dyn workforce_common::settings::SecretStore> {
    Arc::new(crate::user_config::LayeredUserVault::new(
        agent_vault,
        global_vault,
    ))
}

fn discover_workspaces(
    store: &dyn workforce_common::settings::SecretStore,
    prefix: &str,
) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut names: Vec<String> = store
        .list(Some(prefix))
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(prefix)
                .and_then(|rest| rest.split('/').next())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// ─── Vault-Aware Loading ─────────────────────────────────────────────────────

/// Result of loading a skill from the vault.
#[derive(Debug)]
pub struct SkillLoadResult {
    /// Tool names that were registered.
    pub tool_names: Vec<String>,
    /// Human-readable context about the loaded configuration (accounts, workspaces, etc.).
    /// Injected into the agent's system prompt so it knows what's available.
    pub context_hint: Option<String>,
}

/// Built-in skill names with credentials present in either vault — the
/// adapters a turn or round loads before its tools are narrowed. The SKILL.md
/// scanner doesn't see Rust adapters, so this catalog-driven probe is the
/// only way to learn that e.g. `linear` is configured.
///
/// Probes through the same `LayeredUserVault` the skill loaders read, so
/// per-user creds (`users/{uid}/skills/...`, written by My Settings) count
/// when `CURRENT_USER_ID` is in scope. A raw two-vault scan misses those:
/// the skill then loads fine in the agent loop but is never loaded for a
/// round, and /cc reports "jira is exposed but not loaded" while the same
/// agent uses jira happily in Slack.
///
/// `slack` is excluded: SlackManager loads it through a separate path.
pub fn agent_configured_builtin_skills(
    agent_vault: Option<&Arc<Vault>>,
    global_vault: Option<&Arc<Vault>>,
) -> Vec<String> {
    let store: Option<Arc<dyn workforce_common::settings::SecretStore>> =
        match (agent_vault, global_vault) {
            (Some(agent), global) => Some(layered_user_store(Arc::clone(agent), global.cloned())),
            // No agent vault: layer over the global vault alone so per-user
            // overlays (`users/{uid}/...`) still resolve.
            (None, Some(global)) => Some(layered_user_store(Arc::clone(global), None)),
            (None, None) => None,
        };
    let Some(store) = store else {
        return Vec::new();
    };
    let any_under = |prefix: &str| !prefix.is_empty() && !store.list(Some(prefix)).is_empty();
    let exact_set = |path: &str| store.get(path).is_some();
    let credential_configured = |c: &CredentialMeta| {
        // Placeholder paths like `.../workspaces/default/refresh_token`
        // resolve to a literal "default" workspace; probe their parent
        // prefix instead so any real workspace counts.
        if c.vault_path.contains("/*/") || c.vault_path.contains("/default/") {
            c.vault_path
                .rsplit_once('/')
                .map(|(p, _)| any_under(&format!("{p}/")))
                .unwrap_or(false)
        } else {
            exact_set(c.vault_path)
        }
    };

    builtin_skill_catalog()
        .into_iter()
        .filter(|meta| meta.name != "slack")
        .filter(|meta| {
            meta.required_credentials.iter().any(credential_configured)
                || any_under(meta.workspace_vault_prefix)
                // Catches setups outside the catalog's enumerated paths
                // (e.g. `skills/google/oauth_client/client_id`).
                || any_under(&format!("skills/{}/", meta.name))
        })
        .map(|meta| meta.name.to_string())
        .collect()
}

/// Auto-load every built-in skill whose shared credentials are already present
/// in the vault, registering its tools and marking it loaded in `skill_state`.
///
/// The hub's [`SkillStateTracker`](crate::skill_state::SkillStateTracker) is
/// in-memory and rebuilt empty on every boot, so without this a redeploy drops
/// every previously-enabled skill: the agent then sees `[ready]`/`[not
/// configured]` in its prompt and tells the user to set the skill up again.
/// This restores them at startup, mirroring the per-skill, fail-open,
/// idempotent loop in `AgentService::ensure_adapters_loaded` — but for the
/// main agent and every configured skill, not just worker-read adapters.
///
/// Skipped:
/// - Skills already loaded (idempotent).
/// - `obscura` — opt-in only; hidden from the prompt so agents don't auto-use it.
/// - OAuth-managed / per-user skills (e.g. `google`): at boot there is no
///   `CURRENT_USER_ID` task-local, so their `users/{uid}/...` credentials can't
///   be resolved and a load would bind the shared OAuth client with no user
///   token. They stay on-demand, resolved per-request.
///
/// A loaded skill's tools are like any others to the tool plan: resident when
/// the agent uses them, otherwise in the catalogue for `tool_load`.
pub fn auto_load_configured_skills(
    vault: &Arc<Vault>,
    global_vault: Option<&Arc<Vault>>,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
    skill_state: &crate::skill_state::SkillStateTracker,
) {
    let configured = agent_configured_builtin_skills(Some(vault), global_vault);
    let catalog = builtin_skill_catalog();
    for skill in configured {
        if skill_state.is_loaded(&skill) {
            continue;
        }
        if skill == "obscura" {
            continue;
        }
        // Per-user / OAuth-managed skills can't boot-load (no user scope yet).
        if let Some(meta) = catalog.iter().find(|m| m.name == skill.as_str()) {
            if meta.default_scope == CredScope::PerUser || meta.managed_by == CredManaged::Oauth {
                continue;
            }
        }
        match load_and_track(
            &skill,
            Arc::clone(vault),
            global_vault.cloned(),
            config,
            registry,
            skill_state,
            "vault",
        ) {
            Ok(result) => {
                let count = result.tool_names.len();
                info!(skill = %skill, count, "Auto-loaded configured skill at startup");
            }
            Err(e) => {
                warn!(skill = %skill, error = %e, "Failed to auto-load configured skill at startup");
            }
        }
    }
}

/// Tool names a built-in adapter registers, sourced from its static
/// `tool_names()` without loading credentials. Empty for unknown skills.
pub fn static_skill_tool_names(skill_name: &str) -> Vec<&'static str> {
    use workforce_skills::adapters;
    match skill_name {
        "github" => adapters::github::GithubSkill::tool_names(),
        "google" => adapters::google::tool_names(),
        "linear" => adapters::linear::LinearSkill::tool_names(),
        "jira" => adapters::jira::JiraSkill::tool_names(),
        "datadog" => adapters::datadog::DatadogSkill::tool_names(),
        "metabase" => adapters::metabase::MetabaseSkill::tool_names(),
        "xero" => adapters::xero::XeroSkill::tool_names(),
        "freeagent" => adapters::freeagent::FreeAgentSkill::tool_names(),
        "spotify" => adapters::spotify::SpotifySkill::tool_names(),
        "postmark" => adapters::postmark::PostmarkSkill::tool_names(),
        "sendgrid" => adapters::sendgrid::SendGridSkill::tool_names(),
        "hubspot" => adapters::hubspot::HubspotSkill::tool_names(),
        "notion" => adapters::notion::NotionSkill::tool_names(),
        "browser" => adapters::browser::BrowserSkill::tool_names(),
        "obscura" => adapters::obscura::ObscuraSkill::tool_names(),
        "email" => adapters::email::EmailSkill::tool_names(),
        _ => Vec::new(),
    }
}

/// Record the instances a multi-instance skill exposes on the registry it loads
/// into, which belongs to one agent, or forget them when it exposes one.
fn record_instances(registry: &ToolRegistry, skill: &str, variants: Option<SkillVariants>) {
    match variants {
        Some(variants) => registry.set_instance_source(skill, Arc::new(variants)),
        None => registry.clear_instance_source(skill),
    }
}

/// Load a specific skill's tools using vault credentials.
pub fn load_skill_from_vault(
    skill_name: &str,
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    match skill_name {
        "github" => load_github_from_vault(vault, global_vault, config, registry),
        "google" => load_google_from_vault(vault, global_vault, registry),
        "linear" => load_linear_from_vault(vault, global_vault, config, registry),
        "jira" => load_jira_from_vault(vault, global_vault, config, registry),
        "datadog" => load_datadog_from_vault(vault, global_vault, config, registry),
        "metabase" => load_metabase_from_vault(vault, global_vault, registry),
        "xero" => load_xero_from_vault(vault, global_vault, config, registry),
        "freeagent" => load_freeagent_from_vault(vault, global_vault, config, registry),
        "spotify" => load_spotify_from_vault(vault, global_vault, config, registry),
        "postmark" => load_postmark_from_vault(vault, global_vault, config, registry),
        "sendgrid" => load_sendgrid_from_vault(vault, global_vault, config, registry),
        "hubspot" => load_hubspot_from_vault(vault, global_vault, config, registry),
        "notion" => load_notion_from_vault(vault, global_vault, config, registry),
        "browser" => load_browser_from_vault(vault, global_vault, config, registry),
        "obscura" => load_obscura_from_vault(vault, global_vault, config, registry),
        "email" => load_email_from_vault(vault, global_vault, config, registry),
        _ => Err(eyre::eyre!("Unknown built-in skill: {}", skill_name)),
    }
}

/// The instance the skill will use when a call names none, by the adapters'
/// rule over the instances the loader built; `None` marks nothing as default.
fn effective_default<V>(
    instances: &std::collections::HashMap<String, V>,
    selector: Option<&str>,
) -> Option<String> {
    workforce_skills::adapters::utils::preferred_instance(instances, selector).map(str::to_string)
}

fn github_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
    config: &WorkforceConfig,
) -> eyre::Result<(
    workforce_skills::adapters::github::GithubSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/github/workspaces/";
    let suffix = "/token";

    let workspaces: std::collections::HashMap<String, _> = store
        .list(Some(prefix))
        .into_iter()
        .filter_map(|path| {
            let name = path.strip_prefix(prefix)?.strip_suffix(suffix)?.to_string();
            let token = store.get(&path)?;
            let base_url = shared_get(store, &format!("{prefix}{name}/base_url"))
                .unwrap_or_else(|| config.github.base_url.clone());
            Some((
                name.clone(),
                workforce_skills::adapters::github::config::GithubWorkspaceConfig {
                    token,
                    base_url,
                    default_owner: Some(name),
                },
            ))
        })
        .collect();

    let default_workspace = shared_get(store, "skills/github/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());

    let context_hint = if !workspaces.is_empty() {
        let mut parts: Vec<String> = workspaces
            .keys()
            .map(|name| {
                let is_default = effective_default.as_deref() == Some(name.as_str());
                if is_default {
                    format!("\"{}\" (default)", name)
                } else {
                    format!("\"{}\"", name)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Workspaces: {}", parts.join(", "));
        if workspaces.len() > 1 {
            hint.push_str(
                "\nUse the `owner` parameter on GitHub tools to select which org/workspace.",
            );
        }
        Some(hint)
    } else {
        // Single-token mode — still show that GitHub is configured
        Some(
            "Single-token mode. To add another org, use: \
             vault.set('skills/github/workspaces/<org>/token', '<pat>')"
                .to_string(),
        )
    };

    let skill_config = if workspaces.is_empty() {
        // Fall back to single-token mode (backward compat)
        let token = store.get("skills/github/token").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/github/token', '<pat>') for single token, or\n  \
                 - vault.set('skills/github/workspaces/<org>/token', '<pat>') for multi-org"
            )
        })?;
        workforce_skills::adapters::github::GithubSkillConfig {
            token: Some(token),
            base_url: config.github.base_url.clone(),
            default_owner: config.github.default_owner.clone(),
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
        }
    } else {
        let ws_count = workspaces.len();
        info!(skill = "github", workspaces = ws_count, "Multi-org mode");
        workforce_skills::adapters::github::GithubSkillConfig {
            token: None,
            base_url: config.github.base_url.clone(),
            default_owner: None,
            workspaces,
            default_workspace,
        }
    };

    Ok((skill_config, context_hint))
}

fn load_github_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = github_vault_config(store.as_ref(), config)?;
    let skill = workforce_skills::adapters::github::GithubSkill::with_store(skill_config, store)?;
    record_instances(registry, "github", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "github",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_google_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    use workforce_skills::adapters::google::api::{self, GoogleApi, GoogleApiContext};
    use workforce_skills::adapters::google::oauth::OAuthClientCreds;
    use workforce_skills::adapters::google::token_store::{self, TokenStore};

    let store = layered_user_store(Arc::clone(&vault), global_vault);

    // Load OAuth client credentials (bundled or BYO from vault).
    let creds = match OAuthClientCreds::from_store_or_bundled(store.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Google OAuth client not configured — the one-time admin step (vault.set skills/google/oauth_client/{{client_id,client_secret}}) must complete before any user can call google.setup(action='login')");
            OAuthClientCreds {
                client_id: String::new(),
                client_secret: String::new(),
                redirect_uri: None,
            }
        }
    };

    let http = api::build_http_client()?;

    // Discover workspaces by refresh_token presence.
    let workspace_names = token_store::list_workspaces(store.as_ref());

    // Legacy migration: if only skills/google/account exists (old gog path)
    // and no new workspaces are configured, warn.
    if workspace_names.is_empty() {
        if let Some(legacy_email) = vault.get("skills/google/account") {
            warn!(
                email = %legacy_email,
                "Legacy Google account found (skills/google/account). Call google.setup(action='login', no_browser=true) to re-authorize with the new pure-Rust skill."
            );
        }
    }

    let default_ws = vault.get("skills/google/default_workspace");

    // Build GoogleApi per workspace.
    let mut account_apis: std::collections::HashMap<String, Arc<GoogleApi>> =
        std::collections::HashMap::new();
    for ws_name in &workspace_names {
        let ts = TokenStore::new(store.clone(), ws_name.clone(), creds.clone(), http.clone());
        let api = Arc::new(GoogleApi::new(http.clone(), ts));
        account_apis.insert(ws_name.clone(), api);
    }

    // Determine the default API.
    let default_api = if let Some(ref dw) = default_ws {
        account_apis.get(dw).cloned()
    } else {
        account_apis.values().next().cloned()
    };
    let default_api = default_api.unwrap_or_else(|| {
        let ts = TokenStore::new(
            store.clone(),
            "default".to_string(),
            creds.clone(),
            http.clone(),
        );
        Arc::new(GoogleApi::new(http.clone(), ts))
    });

    let ctx = Arc::new(GoogleApiContext::multi(default_api, account_apis));

    // Build all service tools.
    let tools = workforce_skills::adapters::google::services::build_all(ctx);
    let mut names: Vec<String> = Vec::with_capacity(tools.len() + 1);
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }

    // Register the setup tool, wired to the hub's process-wide web-flow state
    // store so `action='login'` can emit a browser redirect URL when
    // `skills/google/oauth_client/redirect_uri` is configured.
    let setup_tool = Arc::new(
        workforce_skills::adapters::google::setup::GoogleSetupTool::new(store.clone())
            .with_web_flow_store(crate::google_oauth_callback::google_web_flow_store()),
    );
    names.push(setup_tool.name().to_string());
    registry.register(setup_tool);

    // Context hint for agent system prompt.
    let context_hint = if !workspace_names.is_empty() {
        let mut parts: Vec<String> = workspace_names
            .iter()
            .map(|ws| {
                let email = store
                    .get(&format!("skills/google/workspaces/{ws}/email"))
                    .unwrap_or_else(|| "(unknown)".to_string());
                let is_default = default_ws.as_deref() == Some(ws.as_str());
                if is_default {
                    format!("\"{}\" = {} (default)", ws, email)
                } else {
                    format!("\"{}\" = {}", ws, email)
                }
            })
            .collect();
        parts.sort();
        Some(format!(
            "Accounts: {}\nUse the `account` parameter on any Google tool to select which account.",
            parts.join(", ")
        ))
    } else {
        Some(
            "No Google account connected for this user yet. To connect one:\n\
             1. Call google.setup(action='login', no_browser=true, workspace='default') — \
                it returns a `user_code` and a `verification_url` (google.com/device).\n\
             2. Present BOTH to the user verbatim and ask them to open the URL and \
                enter the code.\n\
             3. Call google.setup(action='login_poll', workspace='default') — repeat \
                every few seconds until status='authorized' (or 'expired'/'denied').\n\
             If you get 'No Google OAuth client configured', a human admin must first \
             set skills/google/oauth_client/{client_id,client_secret} in the hub's \
             GLOBAL vault from the CLI (`workforce vault set ...` on the hub host) \
             — these are org-wide and cannot be set via the agent vault.set tool."
                .to_string(),
        )
    };

    info!(
        skill = "google",
        count = names.len(),
        workspaces = workspace_names.len(),
        "Loaded Google skill (pure-Rust)"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn linear_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::linear::LinearSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/linear/workspaces/";
    let suffix = "/api_key";

    let mut workspaces = std::collections::HashMap::new();
    let mut prefix_map = std::collections::HashMap::new();

    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(api_key) = store.get(&path) else {
            continue;
        };
        let team_id_path = format!("{prefix}{name}/default_team_id");
        workspaces.insert(
            name.to_string(),
            workforce_skills::adapters::linear::config::LinearWorkspaceConfig {
                api_key,
                base_url: "https://api.linear.app/graphql".to_string(),
                default_team_id: shared_get(store, &team_id_path),
            },
        );

        // Optional team_keys for prefix auto-detection (e.g. "DW" or "STA,STAR")
        let team_keys_path = format!("{prefix}{name}/team_keys");
        if let Some(keys_str) = shared_get(store, &team_keys_path) {
            for key in keys_str.split(',') {
                let key = key.trim().to_uppercase();
                if !key.is_empty() {
                    prefix_map.insert(key, name.to_string());
                }
            }
        }
    }

    let default_workspace = shared_get(store, "skills/linear/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());

    let context_hint = if !workspaces.is_empty() {
        let mut parts: Vec<String> = workspaces
            .keys()
            .map(|name| {
                let prefixes: Vec<&String> = prefix_map
                    .iter()
                    .filter(|(_, ws)| ws.as_str() == name)
                    .map(|(k, _)| k)
                    .collect();
                let is_default = effective_default.as_deref() == Some(name.as_str());
                let prefix_str = if prefixes.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({})",
                        prefixes
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if is_default {
                    format!("\"{}\"{} (default)", name, prefix_str)
                } else {
                    format!("\"{}\"{}", name, prefix_str)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Workspaces: {}", parts.join(", "));
        if !prefix_map.is_empty() {
            hint.push_str("\nTicket prefixes auto-resolve to the correct workspace.");
        } else if workspaces.len() > 1 {
            hint.push_str(
                "\nUse the `workspace` parameter on Linear tools to select which workspace.",
            );
        }
        Some(hint)
    } else {
        // Single-key mode — still show that Linear is configured
        Some(
            "Single-key mode. To add another workspace, use: \
             vault.set('skills/linear/workspaces/<name>/api_key', '<key>')"
                .to_string(),
        )
    };

    let skill_config = if workspaces.is_empty() {
        // Fall back to single-key mode (backward compat)
        let api_key = store.get("skills/linear/api_key").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/linear/api_key', '<key>') for single workspace, or\n  \
                 - vault.set('skills/linear/workspaces/<name>/api_key', '<key>') for multi-workspace"
            )
        })?;
        let default_team_id = shared_get(store, "skills/linear/default_team_id");
        workforce_skills::adapters::linear::LinearSkillConfig {
            api_key: Some(api_key),
            base_url: "https://api.linear.app/graphql".to_string(),
            default_team_id,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
            prefix_map: std::collections::HashMap::new(),
        }
    } else {
        let ws_count = workspaces.len();
        info!(
            skill = "linear",
            workspaces = ws_count,
            prefix_count = prefix_map.len(),
            "Multi-workspace mode"
        );
        workforce_skills::adapters::linear::LinearSkillConfig {
            api_key: None,
            base_url: "https://api.linear.app/graphql".to_string(),
            default_team_id: None,
            workspaces,
            default_workspace,
            prefix_map,
        }
    };

    Ok((skill_config, context_hint))
}

fn load_linear_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = linear_vault_config(store.as_ref())?;
    let skill = workforce_skills::adapters::linear::LinearSkill::with_store(skill_config, store)?;
    record_instances(registry, "linear", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "linear",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn jira_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::jira::JiraSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/jira/workspaces/";
    let suffix = "/api_token";

    let mut workspaces = std::collections::HashMap::new();
    let mut prefix_map = std::collections::HashMap::new();

    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(api_token) = store.get(&path) else {
            continue;
        };
        let Some(email) = store.get(&format!("{prefix}{name}/email")) else {
            warn!(workspace = name, "Skipping Jira workspace: missing email");
            continue;
        };
        let site = shared_get(store, &format!("{prefix}{name}/site"));
        let default_project = shared_get(store, &format!("{prefix}{name}/default_project"));

        let project_keys: Vec<String> = shared_get(store, &format!("{prefix}{name}/project_keys"))
            .into_iter()
            .flat_map(|s| {
                s.split(',')
                    .map(|k| k.trim().to_uppercase())
                    .filter(|k| !k.is_empty())
                    .collect::<Vec<_>>()
            })
            .collect();
        for key in &project_keys {
            prefix_map.insert(key.clone(), name.to_string());
        }

        workspaces.insert(
            name.to_string(),
            workforce_skills::adapters::jira::JiraWorkspaceConfig {
                email,
                api_token,
                site,
                default_project,
                project_keys,
            },
        );
    }

    let default_workspace = shared_get(store, "skills/jira/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());

    let context_hint = if !workspaces.is_empty() {
        let mut parts: Vec<String> = workspaces
            .keys()
            .map(|name| {
                let prefixes: Vec<&String> = prefix_map
                    .iter()
                    .filter(|(_, ws)| ws.as_str() == name)
                    .map(|(k, _)| k)
                    .collect();
                let is_default = effective_default.as_deref() == Some(name.as_str());
                let prefix_str = if prefixes.is_empty() {
                    String::new()
                } else {
                    format!(
                        " ({})",
                        prefixes
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if is_default {
                    format!("\"{}\"{} (default)", name, prefix_str)
                } else {
                    format!("\"{}\"{}", name, prefix_str)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Jira workspaces: {}", parts.join(", "));
        if !prefix_map.is_empty() {
            hint.push_str(
                "\nIssue keys auto-resolve to the correct workspace via project-key prefixes.",
            );
        } else if workspaces.len() > 1 {
            hint.push_str("\nUse the `workspace` parameter on Jira tools to select which tenant.");
        }
        Some(hint)
    } else {
        Some(
            "Single-tenant mode. To add another tenant, use: \
             vault.set('skills/jira/workspaces/<name>/{email,api_token,site}', '...')"
                .to_string(),
        )
    };

    let skill_config = if workspaces.is_empty() {
        let email = store.get("skills/jira/email").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/jira/email|api_token|site', '...') for single tenant, or\n  \
                 - vault.set('skills/jira/workspaces/<name>/{{email,api_token,site}}', '...') for multi-tenant"
            )
        })?;
        let api_token = store
            .get("skills/jira/api_token")
            .ok_or_else(|| eyre::eyre!("Missing 'skills/jira/api_token'"))?;
        let site = shared_get(store, "skills/jira/site");
        let default_project = shared_get(store, "skills/jira/default_project");

        workforce_skills::adapters::jira::JiraSkillConfig {
            email: Some(email),
            api_token: Some(api_token),
            site,
            default_project,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
            prefix_map: std::collections::HashMap::new(),
        }
    } else {
        let ws_count = workspaces.len();
        info!(
            skill = "jira",
            workspaces = ws_count,
            prefix_count = prefix_map.len(),
            "Multi-tenant mode"
        );
        workforce_skills::adapters::jira::JiraSkillConfig {
            email: None,
            api_token: None,
            site: None,
            default_project: None,
            workspaces,
            default_workspace,
            prefix_map,
        }
    };

    Ok((skill_config, context_hint))
}

fn load_jira_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = jira_vault_config(store.as_ref())?;
    let skill = workforce_skills::adapters::jira::JiraSkill::with_store(skill_config, store)?;
    record_instances(registry, "jira", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "jira",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn datadog_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::datadog::DatadogSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/datadog/accounts/";
    let suffix = "/api_key";

    let mut accounts = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(api_key) = store.get(&path) else {
            continue;
        };
        let Some(app_key) = store.get(&format!("{prefix}{name}/app_key")) else {
            warn!(account = name, "Skipping Datadog account: missing app_key");
            continue;
        };
        let site =
            shared_get(store, &format!("{prefix}{name}/site")).unwrap_or_else(|| "us1".to_string());
        let base_url = shared_get(store, &format!("{prefix}{name}/base_url"));

        accounts.insert(
            name.to_string(),
            workforce_skills::adapters::datadog::DatadogAccountConfig {
                api_key,
                app_key,
                site,
                base_url,
            },
        );
    }

    let default_account = shared_get(store, "skills/datadog/default_account");
    let effective_default = effective_default(&accounts, default_account.as_deref());

    let context_hint = if !accounts.is_empty() {
        let mut parts: Vec<String> = accounts
            .iter()
            .map(|(name, acct)| {
                let is_default = effective_default.as_deref() == Some(name.as_str());
                if is_default {
                    format!("\"{}\" ({}, default)", name, acct.site)
                } else {
                    format!("\"{}\" ({})", name, acct.site)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Datadog accounts: {}", parts.join(", "));
        if accounts.len() > 1 {
            hint.push_str(
                "\nUse the `account` parameter on Datadog tools to select which account.",
            );
        }
        Some(hint)
    } else {
        Some(
            "Single-account mode. To add another account, use: \
             vault.set('skills/datadog/accounts/<name>/{api_key,app_key,site}', '...')"
                .to_string(),
        )
    };

    let skill_config = if accounts.is_empty() {
        let api_key = store.get("skills/datadog/api_key").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/datadog/api_key|app_key', '...') for single account, or\n  \
                 - vault.set('skills/datadog/accounts/<name>/{{api_key,app_key,site}}', '...') for multi-account"
            )
        })?;
        let app_key = store
            .get("skills/datadog/app_key")
            .ok_or_else(|| eyre::eyre!("Missing 'skills/datadog/app_key'"))?;
        let site = shared_get(store, "skills/datadog/site");
        let base_url = shared_get(store, "skills/datadog/base_url");

        workforce_skills::adapters::datadog::DatadogSkillConfig {
            api_key: Some(api_key),
            app_key: Some(app_key),
            site,
            base_url,
            accounts: std::collections::HashMap::new(),
            default_account: None,
        }
    } else {
        let count = accounts.len();
        info!(skill = "datadog", accounts = count, "Multi-account mode");
        workforce_skills::adapters::datadog::DatadogSkillConfig {
            api_key: None,
            app_key: None,
            site: None,
            base_url: None,
            accounts,
            default_account,
        }
    };

    Ok((skill_config, context_hint))
}

fn load_datadog_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = datadog_vault_config(store.as_ref())?;
    let skill = workforce_skills::adapters::datadog::DatadogSkill::with_store(skill_config, store)?;
    record_instances(registry, "datadog", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "datadog",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn metabase_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::metabase::MetabaseSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/metabase/instances/";
    let suffix = "/api_key";

    let mut instances = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(api_key) = store.get(&path) else {
            continue;
        };
        let site_url = shared_get(store, &format!("{prefix}{name}/site_url"));
        let max_query_rows = shared_get(store, &format!("{prefix}{name}/max_query_rows"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::metabase::config::DEFAULT_MAX_QUERY_ROWS);

        instances.insert(
            name.to_string(),
            workforce_skills::adapters::metabase::MetabaseInstanceConfig {
                api_key,
                site_url,
                max_query_rows,
            },
        );
    }

    let default_instance = shared_get(store, "skills/metabase/default_instance");
    let effective_default = effective_default(&instances, default_instance.as_deref());

    let context_hint = if !instances.is_empty() {
        let mut parts: Vec<String> = instances
            .iter()
            .map(|(name, inst)| {
                let site = inst.site_url.as_deref().unwrap_or("per-user site");
                if effective_default.as_deref() == Some(name.as_str()) {
                    format!("\"{name}\" ({site}, default)")
                } else {
                    format!("\"{name}\" ({site})")
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Metabase instances: {}", parts.join(", "));
        if instances.len() > 1 {
            hint.push_str(
                "\nUse the `instance` parameter on Metabase tools to select which instance.",
            );
        }
        Some(hint)
    } else {
        Some(
            "Single-instance mode. To add another instance, use: \
             vault.set('skills/metabase/instances/<name>/{api_key,site_url}', '...')"
                .to_string(),
        )
    };

    let skill_config = if instances.is_empty() {
        // Backward-compat single-instance mode.
        let api_key = store.get("skills/metabase/api_key").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/metabase/api_key', '<mb_...>') + vault.set('skills/metabase/site_url', '<url>') for single instance, or\n  \
                 - vault.set('skills/metabase/instances/<name>/{{api_key,site_url}}', '...') for multi-instance"
            )
        })?;
        let site_url = shared_get(store, "skills/metabase/site_url");
        let max_query_rows = shared_get(store, "skills/metabase/max_query_rows")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::metabase::config::DEFAULT_MAX_QUERY_ROWS);
        workforce_skills::adapters::metabase::MetabaseSkillConfig {
            api_key: Some(api_key),
            site_url,
            max_query_rows,
            instances: std::collections::HashMap::new(),
            default_instance: None,
        }
    } else {
        let count = instances.len();
        info!(skill = "metabase", instances = count, "Multi-instance mode");
        workforce_skills::adapters::metabase::MetabaseSkillConfig {
            api_key: None,
            site_url: None,
            max_query_rows: workforce_skills::adapters::metabase::config::DEFAULT_MAX_QUERY_ROWS,
            instances,
            default_instance,
        }
    };

    Ok((skill_config, context_hint))
}

/// The Metabase clients an agent's vault configures, each paired with the site
/// it talks to.
///
/// Built through the same config path the loader uses, so the sync pass sees
/// exactly the instances and credentials the agent's tools do. `None` when the
/// agent has no Metabase configured, which is the common case.
pub fn metabase_clients_for(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
) -> Vec<(
    String,
    Arc<workforce_skills::adapters::metabase::client::MetabaseClient>,
)> {
    let store = layered_user_store(vault, global_vault);
    let Ok((skill_config, _)) = metabase_vault_config(store.as_ref()) else {
        return Vec::new();
    };
    let sites: Vec<(String, String)> = skill_config
        .resolve()
        .into_iter()
        .filter_map(|(name, inst)| inst.site_url.map(|url| (name, url)))
        .collect();
    let Ok(skill) = workforce_skills::adapters::metabase::MetabaseSkill::with_store(
        skill_config,
        store,
        workforce_common::metabase_schema::store(),
    ) else {
        return Vec::new();
    };
    let clients = skill.clients();
    sites
        .into_iter()
        .filter_map(|(name, site_url)| {
            clients
                .get(&name)
                .map(|client| (site_url, Arc::clone(client)))
        })
        .collect()
}

/// The Metabase instances the vault in scope can query, by name, with each
/// one's normalised site, plus the instance a call without `instance` uses.
/// Read under the caller's `CURRENT_USER_ID`, so it is what that person can
/// reach.
pub fn metabase_sites_for(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
) -> (Vec<(String, String)>, Option<String>) {
    let store = layered_user_store(vault, global_vault);
    let Ok((config, _)) = metabase_vault_config(store.as_ref()) else {
        return (Vec::new(), None);
    };
    let sites: Vec<(String, String)> = config
        .resolve()
        .into_iter()
        .filter_map(|(name, inst)| {
            inst.site_url.map(|url| {
                (
                    name,
                    workforce_skills::adapters::metabase::client::normalised_site(&url),
                )
            })
        })
        .collect();
    let default = config
        .default_instance
        .clone()
        .or_else(|| {
            sites
                .iter()
                .any(|(n, _)| n == "default")
                .then(|| "default".to_string())
        })
        .or_else(|| (sites.len() == 1).then(|| sites[0].0.clone()));
    (sites, default)
}

fn load_metabase_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = metabase_vault_config(store.as_ref())?;
    let skill = workforce_skills::adapters::metabase::MetabaseSkill::with_store(
        skill_config,
        store,
        workforce_common::metabase_schema::store(),
    )?;
    record_instances(registry, "metabase", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "metabase",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_xero_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    use workforce_skills::adapters::xero::{XeroEntityConfig, XeroSkill, XeroSkillConfig};

    let store = layered_user_store(vault, global_vault);
    let prefix = "skills/xero/instances/";
    let suffix = "/client_id";

    let mut entities = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(client_id) = store.get(&path) else {
            continue;
        };
        let Some(client_secret) = store.get(&format!("{prefix}{name}/client_secret")) else {
            warn!(entity = name, "Skipping Xero entity: missing client_secret");
            continue;
        };
        entities.insert(
            name.to_string(),
            XeroEntityConfig {
                client_id,
                client_secret,
                tenant_id: store.get(&format!("{prefix}{name}/tenant_id")),
            },
        );
    }

    let default_entity = shared_get(store.as_ref(), "skills/xero/default_entity");
    let effective_default = effective_default(&entities, default_entity.as_deref());

    let context_hint = if entities.is_empty() {
        Some(
            "Single-entity mode. To add another organisation, use: \
             vault.set('skills/xero/instances/<name>/{client_id,client_secret}', '...')"
                .to_string(),
        )
    } else {
        let mut names: Vec<String> = entities
            .keys()
            .map(|name| {
                if effective_default.as_deref() == Some(name.as_str()) {
                    format!("\"{name}\" (default)")
                } else {
                    format!("\"{name}\"")
                }
            })
            .collect();
        names.sort();
        let mut hint = format!("Xero entities: {}", names.join(", "));
        if entities.len() > 1 {
            hint.push_str("\nUse the `entity` parameter on Xero tools to select one.");
        }
        Some(hint)
    };

    let skill_config = if entities.is_empty() {
        let client_id = store.get("skills/xero/client_id").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/xero/client_id', '<id>') + vault.set('skills/xero/client_secret', '<secret>') for a single entity, or\n  \
                 - vault.set('skills/xero/instances/<name>/{{client_id,client_secret}}', '...') for multiple entities"
            )
        })?;
        let client_secret = store
            .get("skills/xero/client_secret")
            .ok_or_else(|| eyre::eyre!("Missing 'skills/xero/client_secret'"))?;
        XeroSkillConfig {
            client_id: Some(client_id),
            client_secret: Some(client_secret),
            tenant_id: store.get("skills/xero/tenant_id"),
            entities: std::collections::HashMap::new(),
            default_entity: None,
        }
    } else {
        info!(
            skill = "xero",
            entities = entities.len(),
            "Multi-entity mode"
        );
        XeroSkillConfig {
            client_id: None,
            client_secret: None,
            tenant_id: None,
            entities,
            default_entity,
        }
    };

    let skill = XeroSkill::with_store(skill_config, store)?;
    record_instances(registry, "xero", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "xero",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_spotify_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    use workforce_skills::adapters::spotify::setup::SpotifySetupTool;
    use workforce_skills::adapters::spotify::{token_store, SpotifySkill};

    let store = layered_user_store(vault, global_vault);
    let mut names: Vec<String> = Vec::new();

    // Playback tools need the admin OAuth app. When it is unconfigured,
    // register only the setup tool so the onboarding instructions are still
    // reachable from chat.
    match SpotifySkill::with_store(Arc::clone(&store)) {
        Ok(skill) => {
            for tool in skill.tools() {
                names.push(tool.name().to_string());
                registry.register(tool);
            }
        }
        Err(e) => warn!(
            error = %e,
            "Spotify OAuth app not configured — only spotify.setup is available until the \
             one-time admin step (vault.set skills/spotify/oauth_client/{{client_id,client_secret,redirect_uri}}) completes"
        ),
    }

    let setup_tool = Arc::new(
        SpotifySetupTool::new(Arc::clone(&store))
            .with_state_store(crate::spotify_oauth_callback::spotify_state_store()),
    );
    names.push(setup_tool.name().to_string());
    registry.register(setup_tool);

    let context_hint = Some(if token_store::is_connected(store.as_ref()) {
        "Spotify is connected; spotify.* tools can control playback.".to_string()
    } else {
        "No Spotify account connected yet. Run spotify.setup(action='login') to connect one."
            .to_string()
    });
    info!(
        skill = "spotify",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_freeagent_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    use workforce_skills::adapters::freeagent::setup::FreeAgentSetupTool;
    use workforce_skills::adapters::freeagent::{token_store, FreeAgentSkill};

    let store = layered_user_store(vault, global_vault);
    let mut names: Vec<String> = Vec::new();

    // Data tools need the admin OAuth app. When it's unconfigured, register
    // only the setup tool so users can still see the onboarding instructions.
    match FreeAgentSkill::with_store(Arc::clone(&store)) {
        Ok(skill) => {
            for tool in skill.tools() {
                names.push(tool.name().to_string());
                registry.register(tool);
            }
        }
        Err(e) => warn!(
            error = %e,
            "FreeAgent OAuth app not configured — only freeagent.setup is available until the \
             one-time admin step (vault.set skills/freeagent/oauth_client/{{client_id,client_secret,redirect_uri}}) completes"
        ),
    }

    // Always register the setup tool, wired to the hub's shared state store so
    // `action='login'` can emit a browser redirect URL.
    let setup_tool = Arc::new(
        FreeAgentSetupTool::new(Arc::clone(&store))
            .with_state_store(crate::freeagent_oauth_callback::freeagent_state_store()),
    );
    names.push(setup_tool.name().to_string());
    registry.register(setup_tool);

    let connected = token_store::list_workspaces(store.as_ref());
    let context_hint = if connected.is_empty() {
        Some(
            "No FreeAgent company connected yet. Run freeagent.setup(action='login') to connect one."
                .to_string(),
        )
    } else {
        Some(format!(
            "Connected FreeAgent companies: {}. Use the `workspace` parameter on freeagent.* tools to select one.",
            connected.join(", ")
        ))
    };
    info!(
        skill = "freeagent",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn postmark_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::postmark::PostmarkSkillConfig,
    Option<String>,
)> {
    let prefix = "skills/postmark/workspaces/";
    let suffix = "/server_token";

    let mut workspaces = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(server_token) = store.get(&path) else {
            continue;
        };
        let account_token = store.get(&format!("{prefix}{name}/account_token"));
        let base_url = shared_get(store, &format!("{prefix}{name}/base_url"));

        workspaces.insert(
            name.to_string(),
            workforce_skills::adapters::postmark::PostmarkWorkspaceConfig {
                server_token,
                account_token,
                base_url,
            },
        );
    }

    let default_workspace = shared_get(store, "skills/postmark/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());

    let context_hint = if !workspaces.is_empty() {
        let mut parts: Vec<String> = workspaces
            .iter()
            .map(|(name, ws)| {
                let is_default = effective_default.as_deref() == Some(name.as_str());
                let acct = if ws.account_token.is_some() {
                    "+account"
                } else {
                    "server-only"
                };
                if is_default {
                    format!("\"{name}\" ({acct}, default)")
                } else {
                    format!("\"{name}\" ({acct})")
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Postmark workspaces: {}", parts.join(", "));
        if workspaces.len() > 1 {
            hint.push_str(
                "\nUse the `workspace` parameter on Postmark tools to select which workspace.",
            );
        }
        hint.push_str(
            "\nAccount-scoped tools (servers_list, domains_list, domain_get, postmark.api with \
             token_type=account) require the account_token to be set on the workspace.",
        );
        Some(hint)
    } else {
        Some(
            "Single-workspace mode. To add another workspace, use: \
             vault.set('skills/postmark/workspaces/<name>/server_token', '...')"
                .to_string(),
        )
    };

    let skill_config = if workspaces.is_empty() {
        // Backward-compat single-workspace mode.
        let server_token = store.get("skills/postmark/server_token").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/postmark/server_token', '<pm_...>') for single workspace, or\n  \
                 - vault.set('skills/postmark/workspaces/<name>/server_token', '...') for multi-workspace"
            )
        })?;
        let account_token = store.get("skills/postmark/account_token");
        let base_url = shared_get(store, "skills/postmark/base_url");

        workforce_skills::adapters::postmark::PostmarkSkillConfig {
            server_token: Some(server_token),
            account_token,
            base_url,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
        }
    } else {
        let count = workspaces.len();
        info!(
            skill = "postmark",
            workspaces = count,
            "Multi-workspace mode"
        );
        workforce_skills::adapters::postmark::PostmarkSkillConfig {
            server_token: None,
            account_token: None,
            base_url: None,
            workspaces,
            default_workspace,
        }
    };

    Ok((skill_config, context_hint))
}

fn load_postmark_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = postmark_vault_config(store.as_ref())?;
    let skill =
        workforce_skills::adapters::postmark::PostmarkSkill::with_store(skill_config, store)?;
    record_instances(registry, "postmark", skill.variants());
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "postmark",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

/// Build the SendGrid skill config and prompt hint from the vault. The API
/// key and sender resolve per request inside the client, so per-user
/// overlays apply there; the host (`base_url`), the default workspace and
/// the advertised senders are shared configuration and are read from the
/// shared tiers only.
fn sendgrid_vault_config(
    store: &dyn workforce_common::settings::SecretStore,
) -> eyre::Result<(
    workforce_skills::adapters::sendgrid::SendGridSkillConfig,
    String,
)> {
    use workforce_skills::adapters::sendgrid::{SendGridSkillConfig, SendGridWorkspaceConfig};

    let prefix = "skills/sendgrid/workspaces/";
    let workspace = |name: &str| {
        let shared = |f: &str| shared_get(store, &format!("{prefix}{name}/{f}"));
        Some(SendGridWorkspaceConfig {
            api_key: store.get(&format!("{prefix}{name}/api_key"))?,
            from_email: shared("from_email"),
            from_name: shared("from_name"),
            base_url: shared("base_url"),
        })
    };
    let workspaces: std::collections::HashMap<String, SendGridWorkspaceConfig> = store
        .list(Some(prefix))
        .into_iter()
        .filter_map(|path| {
            path.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix("/api_key"))
                .map(str::to_string)
        })
        .filter_map(|name| workspace(&name).map(|ws| (name, ws)))
        .collect();
    let sender_note = |from: Option<&str>| {
        from.map_or(
            "no default From — pass `from` on every send".to_string(),
            |from| format!("default From {from}"),
        )
    };

    if workspaces.is_empty() {
        let api_key = store.get("skills/sendgrid/api_key").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/sendgrid/api_key', '<SG....>') for single workspace, or\n  \
                 - vault.set('skills/sendgrid/workspaces/<name>/api_key', '...') for multi-workspace"
            )
        })?;
        let from_email = shared_get(store, "skills/sendgrid/from_email");
        let hint = format!(
            "Single-workspace mode ({}). To add another workspace, use: \
             vault.set('skills/sendgrid/workspaces/<name>/api_key', '...')",
            sender_note(from_email.as_deref())
        );
        return Ok((
            SendGridSkillConfig {
                api_key: Some(api_key),
                from_email,
                from_name: shared_get(store, "skills/sendgrid/from_name"),
                base_url: shared_get(store, "skills/sendgrid/base_url"),
                workspaces,
                default_workspace: None,
            },
            hint,
        ));
    }

    info!(
        skill = "sendgrid",
        workspaces = workspaces.len(),
        "Multi-workspace mode"
    );
    let default_workspace = shared_get(store, "skills/sendgrid/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());
    let mut parts: Vec<String> = workspaces
        .iter()
        .map(|(name, ws)| {
            let tag = if effective_default.as_deref() == Some(name.as_str()) {
                ", default"
            } else {
                ""
            };
            format!(
                "\"{name}\" ({}{tag})",
                sender_note(ws.from_email.as_deref())
            )
        })
        .collect();
    parts.sort();
    let mut hint = format!("SendGrid workspaces: {}", parts.join(", "));
    if workspaces.len() > 1 {
        hint.push_str("\nUse the `workspace` parameter on SendGrid tools to select which account.");
    }
    hint.push_str(
        "\nSends default to the workspace's From; pass `from` to override with another verified \
         sender.",
    );
    Ok((
        SendGridSkillConfig {
            workspaces,
            default_workspace,
            ..SendGridSkillConfig::default()
        },
        hint,
    ))
}

fn load_sendgrid_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let (skill_config, context_hint) = sendgrid_vault_config(store.as_ref())?;
    let skill =
        workforce_skills::adapters::sendgrid::SendGridSkill::with_store(skill_config, store)?;
    record_instances(registry, "sendgrid", skill.variants());
    let tool_names: Vec<String> = skill
        .tools()
        .into_iter()
        .map(|tool| {
            let name = tool.name().to_string();
            registry.register(tool);
            name
        })
        .collect();
    info!(
        skill = "sendgrid",
        count = tool_names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names,
        context_hint: Some(context_hint),
    })
}

fn load_hubspot_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let prefix = "skills/hubspot/portals/";
    let suffix = "/access_token";

    let mut portals = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(access_token) = store.get(&path) else {
            continue;
        };
        let max_results = shared_get(store.as_ref(), &format!("{prefix}{name}/max_results"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::hubspot::config::DEFAULT_MAX_RESULTS);

        portals.insert(
            name.to_string(),
            workforce_skills::adapters::hubspot::HubspotPortalConfig {
                access_token,
                max_results,
            },
        );
    }

    let default_portal = shared_get(store.as_ref(), "skills/hubspot/default_portal");
    let effective_default = effective_default(&portals, default_portal.as_deref());

    let context_hint = if !portals.is_empty() {
        let mut parts: Vec<String> = portals
            .keys()
            .map(|name| {
                if effective_default.as_deref() == Some(name.as_str()) {
                    format!("\"{}\" (default)", name)
                } else {
                    format!("\"{}\"", name)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("HubSpot portals: {}", parts.join(", "));
        if portals.len() > 1 {
            hint.push_str("\nUse the `portal` parameter on HubSpot tools to select which portal.");
        }
        Some(hint)
    } else {
        Some(
            "Single-portal mode. To add another portal, use: \
             vault.set('skills/hubspot/portals/<name>/access_token', '...')"
                .to_string(),
        )
    };

    let skill_config = if portals.is_empty() {
        let access_token = store.get("skills/hubspot/access_token").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/hubspot/access_token', '<pat-...>') for single portal, or\n  \
                 - vault.set('skills/hubspot/portals/<name>/access_token', '...') for multi-portal"
            )
        })?;
        let max_results = store
            .get("skills/hubspot/max_results")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::hubspot::config::DEFAULT_MAX_RESULTS);
        workforce_skills::adapters::hubspot::HubspotSkillConfig {
            access_token: Some(access_token),
            max_results,
            portals: std::collections::HashMap::new(),
            default_portal: None,
        }
    } else {
        let count = portals.len();
        info!(skill = "hubspot", portals = count, "Multi-portal mode");
        workforce_skills::adapters::hubspot::HubspotSkillConfig {
            access_token: None,
            max_results: workforce_skills::adapters::hubspot::config::DEFAULT_MAX_RESULTS,
            portals,
            default_portal,
        }
    };

    let skill = workforce_skills::adapters::hubspot::HubspotSkill::with_store(skill_config, store)?;
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "hubspot",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_notion_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let prefix = "skills/notion/workspaces/";
    let suffix = "/token";

    let mut workspaces = std::collections::HashMap::new();
    for path in store.list(Some(prefix)) {
        let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        else {
            continue;
        };
        let Some(token) = store.get(&path) else {
            continue;
        };
        let max_results = shared_get(store.as_ref(), &format!("{prefix}{name}/max_results"))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::notion::config::DEFAULT_MAX_RESULTS);

        workspaces.insert(
            name.to_string(),
            workforce_skills::adapters::notion::NotionWorkspaceConfig { token, max_results },
        );
    }

    let default_workspace = shared_get(store.as_ref(), "skills/notion/default_workspace");
    let effective_default = effective_default(&workspaces, default_workspace.as_deref());

    let context_hint = if !workspaces.is_empty() {
        let mut parts: Vec<String> = workspaces
            .keys()
            .map(|name| {
                let is_default = effective_default.as_deref() == Some(name.as_str());
                if is_default {
                    format!("\"{}\" (default)", name)
                } else {
                    format!("\"{}\"", name)
                }
            })
            .collect();
        parts.sort();
        let mut hint = format!("Notion workspaces: {}", parts.join(", "));
        if workspaces.len() > 1 {
            hint.push_str(
                "\nUse the `workspace` parameter on Notion tools to select which workspace.",
            );
        }
        Some(hint)
    } else {
        Some(
            "Single-workspace mode. To add another workspace, use: \
             vault.set('skills/notion/workspaces/<name>/token', '...')"
                .to_string(),
        )
    };

    let skill_config = if workspaces.is_empty() {
        let token = store.get("skills/notion/token").ok_or_else(|| {
            eyre::eyre!(
                "Missing vault credential. Set either:\n  \
                 - vault.set('skills/notion/token', '<ntn_...>') for single workspace, or\n  \
                 - vault.set('skills/notion/workspaces/<name>/token', '...') for multi-workspace"
            )
        })?;
        let max_results = store
            .get("skills/notion/max_results")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(workforce_skills::adapters::notion::config::DEFAULT_MAX_RESULTS);
        workforce_skills::adapters::notion::NotionSkillConfig {
            token: Some(token),
            max_results,
            workspaces: std::collections::HashMap::new(),
            default_workspace: None,
        }
    } else {
        let count = workspaces.len();
        info!(skill = "notion", workspaces = count, "Multi-workspace mode");
        workforce_skills::adapters::notion::NotionSkillConfig {
            token: None,
            max_results: workforce_skills::adapters::notion::config::DEFAULT_MAX_RESULTS,
            workspaces,
            default_workspace,
        }
    };

    let skill = workforce_skills::adapters::notion::NotionSkill::with_store(skill_config, store)?;
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "notion",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

fn load_browser_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let config = workforce_skills::adapters::browser::BrowserSkillConfig::default();
    let store = layered_user_store(vault, global_vault);
    let skill = workforce_skills::adapters::browser::BrowserSkill::with_store(config, store)?;
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "browser",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint: None,
    })
}

fn load_obscura_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    _config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let config = workforce_skills::adapters::obscura::ObscuraSkillConfig::default();
    let store = layered_user_store(vault, global_vault);
    let skill = workforce_skills::adapters::obscura::ObscuraSkill::with_store(config, store)?;
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len());
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }
    info!(
        skill = "obscura",
        count = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint: None,
    })
}

fn load_email_from_vault(
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
) -> eyre::Result<SkillLoadResult> {
    let store = layered_user_store(vault, global_vault);
    let prefix = "skills/email/accounts/";
    let suffix = "/username";

    let mut accounts = std::collections::HashMap::new();

    for path in store.list(Some(prefix)) {
        if let Some(name) = path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_suffix(suffix))
        {
            let username = match store.get(&path) {
                Some(u) => u,
                None => continue,
            };

            let password = match store.get(&format!("{prefix}{name}/password")) {
                Some(p) => p,
                None => {
                    warn!(account = name, "Email account missing password, skipping");
                    continue;
                }
            };

            let imap_host = match store.get(&format!("{prefix}{name}/imap_host")) {
                Some(h) => h,
                None => {
                    warn!(account = name, "Email account missing imap_host, skipping");
                    continue;
                }
            };

            let smtp_host = match store.get(&format!("{prefix}{name}/smtp_host")) {
                Some(h) => h,
                None => {
                    warn!(account = name, "Email account missing smtp_host, skipping");
                    continue;
                }
            };

            let imap_port: u16 = store
                .get(&format!("{prefix}{name}/imap_port"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(993);

            let smtp_port: u16 = store
                .get(&format!("{prefix}{name}/smtp_port"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(587);

            let tls: bool = store
                .get(&format!("{prefix}{name}/tls"))
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true);

            let display_name = store.get(&format!("{prefix}{name}/display_name"));

            let connect_timeout_secs: u16 = store
                .get(&format!("{prefix}{name}/connect_timeout_secs"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(30);

            let command_timeout_secs: u16 = store
                .get(&format!("{prefix}{name}/command_timeout_secs"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);

            accounts.insert(
                name.to_string(),
                workforce_skills::adapters::email::EmailAccountConfig {
                    imap_host,
                    imap_port,
                    smtp_host,
                    smtp_port,
                    username,
                    password,
                    display_name,
                    tls,
                    connect_timeout_secs,
                    command_timeout_secs,
                },
            );
        }
    }

    if accounts.is_empty() {
        // No accounts configured — register only the setup wizard so agents can configure email
        let setup_tool: Arc<dyn Tool> =
            Arc::new(workforce_skills::adapters::email::setup::EmailSetupTool);
        let tool_name = setup_tool.name().to_string();
        registry.register(setup_tool);
        info!(
            skill = "email",
            "No accounts configured — registered email.setup wizard only"
        );
        return Ok(SkillLoadResult {
            tool_names: vec![tool_name],
            context_hint: Some(
                "No email accounts configured. Use email.setup to add one.".to_string(),
            ),
        });
    }

    let default_account = shared_get(store.as_ref(), "skills/email/default_account");

    let context_hint = {
        let mut parts: Vec<String> = accounts
            .keys()
            .map(|name| {
                let username = shared_get(store.as_ref(), &format!("{prefix}{name}/username"))
                    .unwrap_or_else(|| "per-user account".to_string());
                if default_account.as_deref() == Some(name.as_str()) {
                    format!("\"{name}\" = {username} (default)")
                } else {
                    format!("\"{name}\" = {username}")
                }
            })
            .collect();
        parts.sort();
        Some(format!(
            "Accounts: {}\nUse the `account` parameter on email tools to select which account.",
            parts.join(", ")
        ))
    };

    let acct_count = accounts.len();
    let skill_config = workforce_skills::adapters::email::EmailSkillConfig {
        accounts,
        default_account,
    };

    let workspace = std::path::PathBuf::from(&config.hub.data_dir)
        .join("skills")
        .join("email");
    let skill =
        workforce_skills::adapters::email::EmailSkill::with_store(skill_config, workspace, store)?;
    let tools = skill.tools();
    let mut names = Vec::with_capacity(tools.len() + 1);
    for tool in tools {
        names.push(tool.name().to_string());
        registry.register(tool);
    }

    // Also register the setup tool so users can add more accounts
    {
        let setup_tool: Arc<dyn Tool> =
            Arc::new(workforce_skills::adapters::email::setup::EmailSetupTool);
        names.push(setup_tool.name().to_string());
        registry.register(setup_tool);
    }

    info!(
        skill = "email",
        accounts = acct_count,
        tools = names.len(),
        "Loaded skill from vault"
    );
    Ok(SkillLoadResult {
        tool_names: names,
        context_hint,
    })
}

/// Unload a skill by removing all tools with its prefix from the registry.
///
/// Returns the list of tool names that were removed.
/// The registered-tool prefix a skill's tools share: its catalog `tool_prefix`,
/// or the skill name for skills outside the catalog.
fn skill_tool_prefix(skill_name: &str) -> String {
    builtin_skill_catalog()
        .iter()
        .find(|m| m.name == skill_name)
        .map_or(skill_name, |m| m.tool_prefix)
        .to_string()
}

/// Whether any of `skill_name`'s tools is registered. The registry, not
/// `SkillStateTracker`, decides: the tracker is bookkeeping written after a
/// registration, so an entry without registered tools must never count as
/// loaded.
pub fn skill_tools_registered(skill_name: &str, registry: &ToolRegistry) -> bool {
    let prefix = format!("{}.", skill_tool_prefix(skill_name));
    registry.names().iter().any(|n| n.starts_with(&prefix))
}

/// Load `skill_name` from the vault into `registry` and record it in
/// `skill_state` under `source`.
pub fn load_and_track(
    skill_name: &str,
    vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
    config: &WorkforceConfig,
    registry: &ToolRegistry,
    skill_state: &crate::skill_state::SkillStateTracker,
    source: &str,
) -> eyre::Result<SkillLoadResult> {
    let result = load_skill_from_vault(skill_name, vault, global_vault, config, registry)?;
    skill_state.mark_loaded(
        skill_name,
        result.tool_names.clone(),
        source,
        result.context_hint.clone(),
    );
    Ok(result)
}

pub fn unload_skill(skill_name: &str, registry: &ToolRegistry) -> Vec<String> {
    let all_names = registry.names();
    let dot_prefix = format!("{}.", skill_tool_prefix(skill_name));
    let to_remove: Vec<String> = all_names
        .into_iter()
        .filter(|n| n.starts_with(&dot_prefix))
        .collect();

    for name in &to_remove {
        registry.unregister(name);
    }
    registry.clear_instance_source(skill_name);

    if !to_remove.is_empty() {
        info!(
            skill = skill_name,
            removed = to_remove.len(),
            "Unloaded skill tools"
        );
    }

    to_remove
}

// ─── Config/Env-Var Loading (backward compat) ────────────────────────────────

/// Load all built-in skills whose configuration/credentials are available
/// via `WorkforceConfig` or environment variables.
///
/// Returns a flat list of tools to register with the agent. Skills whose
/// credentials are missing are silently skipped (with an info log).
#[allow(dead_code)] // Retained for config/env-based loading path (DW-277 deferred startup)
pub fn load_builtin_skill_tools(config: &WorkforceConfig) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();

    load_github_tools(config, &mut tools);
    // Google skill now uses vault-based OAuth exclusively (load_skill_from_vault).
    load_linear_tools(&mut tools);
    load_browser_tools(&mut tools);

    tools
}

/// Load GitHub tools if a token is configured via config/env var.
#[allow(dead_code)]
fn load_github_tools(config: &WorkforceConfig, tools: &mut Vec<Arc<dyn Tool>>) {
    let token = config.github.token.clone();

    let Some(token) = token else {
        info!("GitHub skill: no token configured (set GITHUB_TOKEN env var or [github] token in config)");
        return;
    };

    let skill_config = workforce_skills::adapters::github::GithubSkillConfig {
        token: Some(token),
        base_url: config.github.base_url.clone(),
        default_owner: config.github.default_owner.clone(),
        workspaces: std::collections::HashMap::new(),
        default_workspace: None,
    };

    match workforce_skills::adapters::github::GithubSkill::new(skill_config) {
        Ok(skill) => {
            let skill_tools = skill.tools();
            let count = skill_tools.len();
            tools.extend(skill_tools);
            info!(count, "GitHub skill tools loaded");
        }
        Err(e) => warn!("Failed to initialize GitHub skill: {}", e),
    }
}

// Legacy `load_google_tools` (env-var path) has been removed.
// Google skill now uses the vault-based OAuth flow exclusively.
// Run `workforce google login` to authorize.

/// Load Linear tools if an API key is configured via env var.
#[allow(dead_code)]
fn load_linear_tools(tools: &mut Vec<Arc<dyn Tool>>) {
    let Some(api_key) = std::env::var("LINEAR_API_KEY").ok() else {
        info!("Linear skill: no API key configured (set LINEAR_API_KEY env var or use vault)");
        return;
    };

    let default_team_id = std::env::var("LINEAR_DEFAULT_TEAM_ID").ok();

    let skill_config = workforce_skills::adapters::linear::LinearSkillConfig {
        api_key: Some(api_key),
        base_url: "https://api.linear.app/graphql".to_string(),
        default_team_id,
        workspaces: std::collections::HashMap::new(),
        default_workspace: None,
        prefix_map: std::collections::HashMap::new(),
    };

    match workforce_skills::adapters::linear::LinearSkill::new(skill_config) {
        Ok(skill) => {
            let skill_tools = skill.tools();
            let count = skill_tools.len();
            tools.extend(skill_tools);
            info!(count, "Linear skill tools loaded");
        }
        Err(e) => warn!("Failed to initialize Linear skill: {}", e),
    }
}

#[allow(dead_code)]
fn load_browser_tools(tools: &mut Vec<Arc<dyn Tool>>) {
    let skill_config = workforce_skills::adapters::browser::BrowserSkillConfig::default();

    match workforce_skills::adapters::browser::BrowserSkill::new(skill_config) {
        Ok(skill) => {
            let skill_tools = skill.tools();
            let count = skill_tools.len();
            tools.extend(skill_tools);
            info!(count, "Browser skill tools loaded");
        }
        Err(e) => warn!("Failed to initialize Browser skill: {}", e),
    }
}

/// Pre-load the configured built-in adapters `wanted` names into the
/// shared tool registry, so a snapshot taken from it next (a worker's
/// filtered registry, an exclusive turn's) includes their tools. Mirrors
/// the loader path in `system.enable_skill` and is idempotent against the
/// registry.
pub(crate) fn ensure_adapters_loaded(service: &AgentService, wanted: impl Fn(&str) -> bool) {
    let Some(vault) = service.vault.as_ref() else {
        return;
    };
    crate::skill_loader::agent_configured_builtin_skills(
        Some(vault),
        service.global_vault.as_ref(),
    )
    .into_iter()
    .filter(|skill| {
        wanted(skill) && !crate::skill_loader::skill_tools_registered(skill, &service.tools)
    })
    .for_each(|skill| {
        match crate::skill_loader::load_and_track(
            &skill,
            Arc::clone(vault),
            service.global_vault.clone(),
            &service.config,
            &service.tools,
            &service.skill_state,
            "vault",
        ) {
            Ok(result) => info!(
                skill = %skill,
                count = result.tool_names.len(),
                "Pre-loaded configured adapter"
            ),
            Err(e) => warn!(skill = %skill, error = %e, "Failed to pre-load adapter"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_common::settings::WorkforceConfig;

    fn new_test_vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        // Keep the tempdir alive — OS cleans up on exit.
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    #[test]
    fn global_skills_list_shows_each_caller_what_they_can_use() {
        use crate::external_skills::tests::{owned, record_in, write_skill};
        use crate::skill_access::SkillCaller;
        use workforce_core::access::Visibility;

        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        for name in ["gl-private", "gl-global", "gl-org", "gl-unreadable"] {
            write_skill(&skills_dir, name, name).unwrap();
        }
        record_in(
            &skills_dir.join("gl-private"),
            &owned(Some("alice"), Visibility::Private, vec![]),
        );
        record_in(
            &skills_dir.join("gl-global"),
            &owned(Some("alice"), Visibility::Global, vec![]),
        );
        std::fs::write(
            skills_dir
                .join("gl-unreadable")
                .join(workforce_skills::installer::ACCESS_FILE),
            "not json",
        )
        .unwrap();
        let broken = skills_dir.join("gl-broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("SKILL.md"), "not a skill yet").unwrap();
        record_in(&broken, &owned(Some("alice"), Visibility::Private, vec![]));

        let listed = |caller: SkillCaller| {
            let mut names: Vec<String> = global_skills_list(&skills_dir, &caller)
                .into_iter()
                .filter(|skill| skill.skill_type == "external" && skill.name.starts_with("gl-"))
                .map(|skill| skill.name)
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            listed(SkillCaller::local(Some("alice".into()), false)),
            ["gl-broken", "gl-global", "gl-org", "gl-private"]
        );
        assert_eq!(
            listed(SkillCaller::local(Some("bob".into()), false)),
            ["gl-global", "gl-org"]
        );
        assert_eq!(
            listed(SkillCaller::local(Some("root".into()), true)),
            [
                "gl-broken",
                "gl-global",
                "gl-org",
                "gl-private",
                "gl-unreadable"
            ]
        );
        assert_eq!(
            listed(SkillCaller::local(None, false)),
            ["gl-global", "gl-org"]
        );
    }

    /// `load_linear_from_vault` resolves credentials through
    /// `LayeredUserVault`, so a `users/<uid>/...` key in the agent vault
    /// loads when `CURRENT_USER_ID` is scoped to that uid.
    #[tokio::test]
    async fn load_linear_uses_user_scoped_credential_when_identity_in_scope() {
        let agent_vault = new_test_vault();
        agent_vault
            .set("users/kings/skills/linear/api_key", "user-scoped-key")
            .unwrap();
        let registry = ToolRegistry::new();
        let config = WorkforceConfig::default();

        let result = CURRENT_USER_ID
            .scope("kings".to_string(), async {
                load_linear_from_vault(agent_vault, None, &config, &registry)
            })
            .await;
        assert!(
            result.is_ok(),
            "user-scoped credential should resolve under CURRENT_USER_ID scope"
        );
        assert!(
            registry.names().iter().any(|n| n.starts_with("linear.")),
            "expected linear tools to be registered"
        );
    }

    /// Without `CURRENT_USER_ID` in scope, only bare-path credentials
    /// resolve — a vault that only has `users/<uid>/...` entries fails the
    /// load (no shared key).
    #[tokio::test]
    async fn load_linear_fails_without_identity_when_only_user_scoped_keys_exist() {
        let agent_vault = new_test_vault();
        agent_vault
            .set("users/kings/skills/linear/api_key", "user-scoped-key")
            .unwrap();
        let registry = ToolRegistry::new();
        let config = WorkforceConfig::default();

        let result = load_linear_from_vault(agent_vault, None, &config, &registry);
        assert!(
            result.is_err(),
            "load should fail when only user-scoped creds exist and no identity is scoped"
        );
    }

    /// Per-user overlays supply the key and sender per request, but the host,
    /// default workspace and advertised senders shape the client every user
    /// shares — a caller's private `base_url` must never leak into it.
    #[tokio::test]
    async fn sendgrid_config_ignores_per_user_host_and_routing_but_loads_a_per_user_key() {
        let vault = new_test_vault();
        vault
            .set("skills/sendgrid/from_email", "noreply@org.com")
            .unwrap();
        vault
            .set("users/mallory/skills/sendgrid/api_key", "SG.mallory")
            .unwrap();
        vault
            .set(
                "users/mallory/skills/sendgrid/base_url",
                "https://attacker.example",
            )
            .unwrap();
        vault
            .set(
                "users/mallory/skills/sendgrid/from_email",
                "mallory@example.com",
            )
            .unwrap();
        let store = layered_user_store(Arc::clone(&vault), None);

        let (config, hint) = CURRENT_USER_ID
            .scope("mallory".to_string(), async {
                sendgrid_vault_config(store.as_ref())
            })
            .await
            .unwrap();
        assert_eq!(config.api_key.as_deref(), Some("SG.mallory"));
        assert!(config.base_url.is_none(), "{config:?}");
        assert_eq!(config.from_email.as_deref(), Some("noreply@org.com"));
        assert!(
            hint.contains("noreply@org.com") && !hint.contains("mallory"),
            "{hint}"
        );

        let err = sendgrid_vault_config(store.as_ref())
            .unwrap_err()
            .to_string();
        assert!(err.contains("skills/sendgrid/api_key"), "{err}");
    }

    #[tokio::test]
    async fn sendgrid_config_discovers_workspaces_with_shared_hosts_only() {
        let vault = new_test_vault();
        vault
            .set("skills/sendgrid/workspaces/eu/api_key", "SG.eu")
            .unwrap();
        vault
            .set(
                "skills/sendgrid/workspaces/eu/base_url",
                "https://api.eu.sendgrid.com",
            )
            .unwrap();
        vault
            .set("skills/sendgrid/workspaces/eu/from_email", "eu@org.com")
            .unwrap();
        vault
            .set("skills/sendgrid/default_workspace", "eu")
            .unwrap();
        vault
            .set(
                "users/mallory/skills/sendgrid/workspaces/mine/api_key",
                "SG.mine",
            )
            .unwrap();
        vault
            .set(
                "users/mallory/skills/sendgrid/workspaces/mine/base_url",
                "https://attacker.example",
            )
            .unwrap();
        vault
            .set("users/mallory/skills/sendgrid/default_workspace", "mine")
            .unwrap();
        let store = layered_user_store(Arc::clone(&vault), None);

        let (config, hint) = CURRENT_USER_ID
            .scope("mallory".to_string(), async {
                sendgrid_vault_config(store.as_ref())
            })
            .await
            .unwrap();
        assert_eq!(config.default_workspace.as_deref(), Some("eu"));
        assert_eq!(
            config.workspaces["eu"].base_url.as_deref(),
            Some("https://api.eu.sendgrid.com")
        );
        assert!(config.workspaces["mine"].base_url.is_none());
        assert!(
            hint.contains("\"eu\" (default From eu@org.com, default)"),
            "{hint}"
        );
        assert!(hint.contains("\"mine\" (no default From"), "{hint}");

        let registry = ToolRegistry::new();
        let loaded =
            load_sendgrid_from_vault(vault, None, &WorkforceConfig::default(), &registry).unwrap();
        assert!(loaded.tool_names.iter().all(|n| n.starts_with("sendgrid.")));
        assert!(registry
            .names()
            .contains(&"sendgrid.email_send".to_string()));
    }

    fn attacker_vault(shared: &[(&str, &str)], per_user: &[(&str, &str)]) -> Arc<Vault> {
        let vault = new_test_vault();
        for (path, value) in shared {
            vault.set(path, value).unwrap();
        }
        for (path, value) in per_user {
            vault.set(&format!("users/mallory/{path}"), value).unwrap();
        }
        vault
    }

    async fn as_mallory<T>(
        vault: &Arc<Vault>,
        f: impl FnOnce(&dyn workforce_common::settings::SecretStore) -> T,
    ) -> T {
        let store = layered_user_store(Arc::clone(vault), None);
        CURRENT_USER_ID
            .scope("mallory".to_string(), async { f(store.as_ref()) })
            .await
    }

    #[tokio::test]
    async fn postmark_config_keeps_shared_hosts_and_routing_under_a_per_user_scope() {
        let vault = attacker_vault(
            &[
                ("skills/postmark/workspaces/org/server_token", "pm_org"),
                (
                    "skills/postmark/workspaces/org/base_url",
                    "https://pm.org.example",
                ),
                ("skills/postmark/default_workspace", "org"),
            ],
            &[
                (
                    "skills/postmark/workspaces/org/base_url",
                    "https://attacker.example",
                ),
                ("skills/postmark/default_workspace", "mine"),
                ("skills/postmark/workspaces/mine/server_token", "pm_mine"),
                (
                    "skills/postmark/workspaces/mine/base_url",
                    "https://attacker.example",
                ),
            ],
        );
        let (config, _) = as_mallory(&vault, |store| postmark_vault_config(store))
            .await
            .unwrap();
        assert_eq!(config.default_workspace.as_deref(), Some("org"));
        assert_eq!(
            config.workspaces["org"].base_url.as_deref(),
            Some("https://pm.org.example")
        );
        assert_eq!(config.workspaces["mine"].server_token, "pm_mine");
        assert!(config.workspaces["mine"].base_url.is_none());
    }

    #[tokio::test]
    async fn jira_config_keeps_shared_sites_and_loads_a_per_user_only_workspace() {
        let vault = attacker_vault(
            &[
                ("skills/jira/workspaces/org/api_token", "tok_org"),
                ("skills/jira/workspaces/org/email", "bot@org.example"),
                (
                    "skills/jira/workspaces/org/site",
                    "https://org.atlassian.net",
                ),
                ("skills/jira/workspaces/org/project_keys", "ORG"),
            ],
            &[
                (
                    "skills/jira/workspaces/org/site",
                    "https://attacker.example",
                ),
                ("skills/jira/workspaces/org/project_keys", "EVIL"),
                ("skills/jira/default_workspace", "mine"),
                ("skills/jira/workspaces/mine/api_token", "tok_mine"),
                ("skills/jira/workspaces/mine/email", "me@example.com"),
                (
                    "skills/jira/workspaces/mine/site",
                    "https://me.atlassian.net",
                ),
            ],
        );
        let (config, hint) = as_mallory(&vault, |store| jira_vault_config(store))
            .await
            .unwrap();
        assert!(config.default_workspace.is_none());
        assert_eq!(
            config.workspaces["org"].site.as_deref(),
            Some("https://org.atlassian.net")
        );
        assert_eq!(
            config.prefix_map.get("ORG").map(String::as_str),
            Some("org")
        );
        assert!(!config.prefix_map.contains_key("EVIL"));
        assert!(
            config.workspaces["mine"].site.is_none(),
            "per-user site resolves per request"
        );
        assert!(hint.unwrap().contains("\"mine\""));

        let registry = ToolRegistry::new();
        let loaded = CURRENT_USER_ID
            .scope("mallory".to_string(), async {
                load_jira_from_vault(vault, None, &WorkforceConfig::default(), &registry)
            })
            .await
            .unwrap();
        assert!(loaded.tool_names.iter().any(|n| n == "jira.search"));
    }

    #[tokio::test]
    async fn datadog_metabase_and_github_configs_keep_shared_hosts_under_a_per_user_scope() {
        let vault = attacker_vault(
            &[
                ("skills/datadog/api_key", "dd_api"),
                ("skills/datadog/app_key", "dd_app"),
                ("skills/datadog/site", "us1"),
                ("skills/metabase/api_key", "mb_key"),
                ("skills/metabase/site_url", "https://mb.org.example"),
                ("skills/github/workspaces/org/token", "ghp_org"),
                (
                    "skills/github/workspaces/org/base_url",
                    "https://ghe.org.example/api/v3",
                ),
            ],
            &[
                ("skills/datadog/site", "eu1"),
                ("skills/datadog/base_url", "https://attacker.example"),
                ("skills/datadog/default_account", "mine"),
                ("skills/metabase/site_url", "https://attacker.example"),
                ("skills/metabase/max_query_rows", "1"),
                (
                    "skills/github/workspaces/org/base_url",
                    "https://attacker.example",
                ),
                ("skills/github/default_workspace", "mine"),
            ],
        );
        let (datadog, _) = as_mallory(&vault, |store| datadog_vault_config(store))
            .await
            .unwrap();
        assert_eq!(datadog.site.as_deref(), Some("us1"));
        assert!(datadog.base_url.is_none());
        assert!(datadog.default_account.is_none());

        let (metabase, _) = as_mallory(&vault, |store| metabase_vault_config(store))
            .await
            .unwrap();
        assert_eq!(metabase.site_url.as_deref(), Some("https://mb.org.example"));
        assert_eq!(
            metabase.max_query_rows,
            workforce_skills::adapters::metabase::config::DEFAULT_MAX_QUERY_ROWS
        );

        let workforce_config = WorkforceConfig::default();
        let (github, github_hint) = as_mallory(&vault, |store| {
            github_vault_config(store, &workforce_config)
        })
        .await
        .unwrap();
        assert_eq!(
            github.workspaces["org"].base_url,
            "https://ghe.org.example/api/v3"
        );
        assert!(github.default_workspace.is_none());
        let github_hint = github_hint.unwrap();
        assert!(github_hint.contains("\"org\" (default)"), "{github_hint}");
        assert!(!github_hint.contains("Use the `owner`"), "{github_hint}");
    }

    #[tokio::test]
    async fn linear_config_keeps_shared_routing_under_a_per_user_scope() {
        let vault = attacker_vault(
            &[
                ("skills/linear/workspaces/org/api_key", "lin_org"),
                ("skills/linear/workspaces/org/team_keys", "ORG"),
                ("skills/linear/default_workspace", "org"),
            ],
            &[
                ("skills/linear/workspaces/org/team_keys", "EVIL"),
                ("skills/linear/workspaces/org/default_team_id", "team-evil"),
                ("skills/linear/default_workspace", "mine"),
            ],
        );
        let (config, _) = as_mallory(&vault, |store| linear_vault_config(store))
            .await
            .unwrap();
        assert_eq!(config.default_workspace.as_deref(), Some("org"));
        assert_eq!(
            config.prefix_map.get("ORG").map(String::as_str),
            Some("org")
        );
        assert!(!config.prefix_map.contains_key("EVIL"));
        assert!(config.workspaces["org"].default_team_id.is_none());
    }

    #[tokio::test]
    async fn metabase_per_user_only_instance_loads_and_resolves_its_site_per_request() {
        let vault = attacker_vault(
            &[],
            &[
                ("skills/metabase/instances/mine/api_key", "mb_mine"),
                (
                    "skills/metabase/instances/mine/site_url",
                    "https://mb.me.example",
                ),
            ],
        );
        let (config, hint) = as_mallory(&vault, |store| metabase_vault_config(store))
            .await
            .unwrap();
        assert!(config.instances["mine"].site_url.is_none());
        let hint = hint.unwrap();
        assert!(hint.contains("\"mine\" (per-user site, default)"), "{hint}");
        assert!(!hint.contains("Use the `instance`"), "{hint}");

        let registry = ToolRegistry::new();
        let loaded = CURRENT_USER_ID
            .scope("mallory".to_string(), async {
                load_metabase_from_vault(vault, None, &registry)
            })
            .await
            .unwrap();
        assert!(loaded.tool_names.iter().any(|n| n == "metabase.card_query"));
    }

    #[test]
    fn default_selectors_are_shared_only_catalog_entries() {
        for (_, path, _) in DEFAULT_SELECTORS {
            assert!(is_default_selector(path), "{path}");
        }
        assert!(!is_default_selector("skills/jira/site"));
        for entry in default_selector_entries() {
            assert_eq!(entry.scope, "shared", "{}", entry.template);
            assert!(!entry.per_user_allowed, "{}", entry.template);
        }
    }

    #[test]
    fn agent_configured_builtin_skills_detects_flat_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        vault.set("skills/linear/api_key", "lk_test").unwrap();

        let configured = agent_configured_builtin_skills(Some(&vault), None);
        assert!(
            configured.iter().any(|s| s == "linear"),
            "expected `linear` in configured set, got {configured:?}"
        );
    }

    #[test]
    fn agent_configured_builtin_skills_detects_workspace_prefix() {
        // Multi-workspace setups don't put a flat key, only nested paths.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        vault
            .set("skills/jira/workspaces/acme/api_token", "tok")
            .unwrap();

        let configured = agent_configured_builtin_skills(Some(&vault), None);
        assert!(
            configured.iter().any(|s| s == "jira"),
            "expected `jira` in configured set, got {configured:?}"
        );
    }

    /// Per-user creds (`users/{uid}/skills/...`, written by My Settings)
    /// must count as "configured" when the caller's identity is scoped —
    /// the skill loaders read through `LayeredUserVault`, so the preload
    /// scan has to probe the same view. Regression: per-user jira creds
    /// loaded fine in the agent loop, but never made the
    /// `--preload-skills` list → /cc "jira exposed but not loaded".
    #[test]
    fn agent_configured_builtin_skills_detects_per_user_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        vault
            .set("users/uid-1/skills/jira/workspaces/acme/api_token", "tok")
            .unwrap();

        let scoped = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("uid-1".to_string(), || {
                agent_configured_builtin_skills(Some(&vault), None)
            });
        assert!(
            scoped.iter().any(|s| s == "jira"),
            "expected per-user `jira` cred to count when identity scoped, got {scoped:?}"
        );

        let unscoped = agent_configured_builtin_skills(Some(&vault), None);
        assert!(
            !unscoped.iter().any(|s| s == "jira"),
            "per-user cred must not leak into an unscoped scan, got {unscoped:?}"
        );
    }

    #[test]
    fn agent_configured_builtin_skills_falls_through_to_global_vault() {
        // `skills/google/oauth_client/*` is org-wide and lives in the global
        // vault by policy. The agent vault may have no google entries.
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join("agent.enc");
        let global_path = dir.path().join("global.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let agent = Arc::new(Vault::open(&agent_path, &key).unwrap());
        let global = Arc::new(Vault::open(&global_path, &key).unwrap());
        global
            .set(
                "skills/google/oauth_client/client_id",
                "cid.apps.googleusercontent.com",
            )
            .unwrap();

        let configured = agent_configured_builtin_skills(Some(&agent), Some(&global));
        assert!(
            configured.iter().any(|s| s == "google"),
            "expected `google` from global vault, got {configured:?}"
        );
    }

    #[test]
    fn agent_configured_builtin_skills_omits_unconfigured_and_slack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        // Slack creds present but excluded from the list — Slack loads via
        // SlackManager hot-reload, not the adapter preload.
        vault.set("slack.W123.bot_token", "xoxb-...").unwrap();

        let configured = agent_configured_builtin_skills(Some(&vault), None);
        assert!(
            !configured.iter().any(|s| s == "slack"),
            "slack must not appear in the preload set even when configured"
        );
        // Linear/jira aren't configured here.
        assert!(
            !configured.iter().any(|s| s == "linear" || s == "jira"),
            "unconfigured skills should be absent: got {configured:?}"
        );
    }

    #[test]
    fn static_skill_tool_names_returns_known_adapter_lists() {
        // Spot-check: each built-in adapter the `/cc` MCP wiring relies on
        // must return a non-empty list with the expected prefix.
        let cases: &[(&str, &str)] = &[
            ("linear", "linear."),
            ("jira", "jira."),
            ("datadog", "datadog."),
            ("metabase", "metabase."),
            ("xero", "xero."),
            ("github", "github."),
            ("google", "google."),
            ("postmark", "postmark."),
            ("sendgrid", "sendgrid."),
            ("hubspot", "hubspot."),
            ("notion", "notion."),
            ("browser", "browser."),
            ("obscura", "obscura."),
            ("email", "email."),
        ];
        for (skill, prefix) in cases {
            let names = static_skill_tool_names(skill);
            assert!(
                !names.is_empty(),
                "expected non-empty tool list for skill {skill}"
            );
            for n in &names {
                assert!(
                    n.starts_with(prefix),
                    "tool {n} for skill {skill} doesn't start with {prefix}"
                );
            }
        }
    }

    #[test]
    fn static_skill_tool_names_unknown_skill_is_empty() {
        assert!(static_skill_tool_names("nope-not-a-skill").is_empty());
    }

    #[test]
    fn dir_name_maps_slack_to_slack_integration() {
        let slack = builtin_skill_catalog()
            .into_iter()
            .find(|m| m.name == "slack")
            .expect("slack in catalog");
        assert_eq!(slack.dir_name(), "slack-integration");
    }

    #[test]
    fn dir_name_is_canonical_for_other_builtins() {
        for meta in builtin_skill_catalog() {
            if meta.name != "slack" {
                assert_eq!(meta.dir_name(), meta.name);
            }
        }
    }

    #[test]
    fn builtin_config_template_returns_slack_template() {
        let tpl =
            builtin_config_template("slack-integration").expect("slack has a default template");
        assert!(tpl.contains("[[workspaces]]"));
        assert!(tpl.contains("response_mode = \"mention_only\""));
        assert!(tpl.contains("thread_isolation = true"));
        assert!(tpl.contains("paragraph_spacing = true"));
    }

    #[test]
    fn builtin_config_template_returns_none_for_skills_without_template() {
        assert!(builtin_config_template("github").is_none());
        assert!(builtin_config_template("google").is_none());
        assert!(builtin_config_template("linear").is_none());
        assert!(builtin_config_template("browser").is_none());
        assert!(builtin_config_template("email").is_none());
    }

    #[test]
    fn builtin_config_template_returns_none_for_unknown_skill() {
        assert!(builtin_config_template("nonexistent").is_none());
    }

    #[test]
    fn slack_default_template_parses_as_valid_toml() {
        let tpl = builtin_config_template("slack-integration").unwrap();
        let parsed: toml::Value = toml::from_str(tpl).expect("slack template is valid TOML");
        let workspaces = parsed
            .get("workspaces")
            .and_then(|v| v.as_array())
            .expect("workspaces array");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(
            workspaces[0].get("id").and_then(|v| v.as_str()),
            Some("default")
        );
        assert_eq!(
            parsed.get("response_mode").and_then(|v| v.as_str()),
            Some("mention_only")
        );
        assert_eq!(
            parsed.get("thread_isolation").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            parsed.get("paragraph_spacing").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn all_builtin_skills_have_unique_names_and_dir_names() {
        let catalog = builtin_skill_catalog();
        let names: std::collections::HashSet<_> = catalog.iter().map(|m| m.name).collect();
        let dirs: std::collections::HashSet<_> = catalog.iter().map(|m| m.dir_name()).collect();
        assert_eq!(names.len(), catalog.len(), "builtin names must be unique");
        assert_eq!(
            dirs.len(),
            catalog.len(),
            "builtin dir_names must be unique"
        );
    }

    // ── Credential catalog (vault editor) ──────────────────────────────────

    #[test]
    fn representative_path_fills_single_variable() {
        assert_eq!(
            representative_path("skills/github/workspaces/{workspace}/token"),
            "skills/github/workspaces/x/token"
        );
        assert_eq!(
            representative_path("slack.{workspace}.bot_token"),
            "slack.x.bot_token"
        );
        // Fixed paths pass through unchanged.
        assert_eq!(
            representative_path("skills/linear/api_key"),
            "skills/linear/api_key"
        );
    }

    #[test]
    fn the_vault_dropdown_offers_every_media_key_once_and_the_speech_keys_as_before() {
        let cat = credential_catalog();
        let mut ids: Vec<&str> = cat.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), total, "a credential is offered twice");

        for (path, label) in [
            ("speech/fish_audio/api_key", "Fish Audio — API key"),
            ("speech/elevenlabs/api_key", "ElevenLabs — API key"),
            ("speech/openai/api_key", "OpenAI speech — API key"),
            ("speech/soniox/api_key", "Soniox — API key"),
            (
                "vision/anthropic/api_key",
                "Image description — Anthropic API key",
            ),
            (
                "vision/openrouter/api_key",
                "Image description — OpenRouter API key",
            ),
            (
                "vision/gemini/api_key",
                "Image description — Gemini API key",
            ),
            (
                "transcription/openai/api_key",
                "Transcription — OpenAI API key",
            ),
            ("transcription/groq/api_key", "Transcription — Groq API key"),
            (
                "transcription/gemini/api_key",
                "Transcription — Gemini API key",
            ),
            (
                "transcription/openrouter/api_key",
                "Transcription — OpenRouter API key",
            ),
            ("image/gemini/api_key", "Image generation — Gemini API key"),
            (
                "image/openrouter/api_key",
                "Image generation — OpenRouter API key",
            ),
            ("video/gemini/api_key", "Video generation — Gemini API key"),
        ] {
            let e = cat
                .iter()
                .find(|e| e.id == path)
                .unwrap_or_else(|| panic!("missing catalog entry {path}"));
            assert_eq!((e.template.as_str(), e.label.as_str()), (path, label));
            assert_eq!(e.group, path.split('/').next().unwrap());
            assert_eq!(
                (e.scope, e.managed_by, e.source),
                ("either", "manual", "builtin")
            );
            assert!(
                e.addable && e.per_user_allowed && e.variables.is_empty(),
                "{path}"
            );
        }
        let summariser = cat
            .iter()
            .find(|e| e.id == "knowledge/summariser/api_key")
            .expect("the summariser key");
        assert_eq!(summariser.scope, "shared");
        assert!(summariser.addable && !summariser.per_user_allowed);
    }

    #[test]
    fn credential_catalog_carries_scope_and_managed_by() {
        let cat = credential_catalog();
        assert!(!cat.is_empty());
        // Google refresh token is OAuth-managed.
        let g = cat
            .iter()
            .find(|e| e.template == "skills/google/workspaces/{workspace}/refresh_token")
            .expect("google workspace refresh_token entry");
        assert_eq!(g.managed_by, "oauth");
        assert_eq!(g.scope, "either");
        assert_eq!(g.variables.len(), 1);
        assert_eq!(g.variables[0].name, "workspace");
        // Slack is shared, not per-user.
        let s = cat
            .iter()
            .find(|e| e.skill.as_deref() == Some("slack"))
            .expect("a slack entry");
        assert_eq!(s.scope, "shared");
        // Speech credentials are plain per-provider secrets, per-user capable,
        // and addable from the vault editor dropdown. Keys may also be shared;
        // voices are per-user only (no shared vault voice path is ever read —
        // the shared voice is [llm.speech].voice config).
        for (path, scope) in [
            ("speech/fish_audio/api_key", "either"),
            ("speech/fish_audio/voice", "per_user"),
            ("speech/elevenlabs/api_key", "either"),
            ("speech/elevenlabs/voice", "per_user"),
            ("speech/openai/api_key", "either"),
            ("speech/openai/voice", "per_user"),
            ("speech/soniox/api_key", "either"),
            ("speech/soniox/voice", "per_user"),
            ("speech/builtin/voice", "per_user"),
        ] {
            let e = cat
                .iter()
                .find(|e| e.template == path)
                .unwrap_or_else(|| panic!("missing speech catalog entry {path}"));
            assert_eq!(e.group, "speech");
            assert!(e.addable);
            assert_eq!(e.scope, scope, "{path}");
            assert!(e.per_user_allowed, "{path} must be per-user capable");
        }
        for (path, scope) in [
            ("decision/jev/api_key", "either"),
            ("decision/kev/api_key", "shared"),
            ("embeddings/openai/api_key", "shared"),
        ] {
            let e = cat
                .iter()
                .find(|e| e.template == path)
                .unwrap_or_else(|| panic!("missing integration catalog entry {path}"));
            assert!(e.addable && e.skill.is_none());
            assert_eq!(e.scope, scope, "{path}");
        }
    }

    /// Mirror of the UI's `matchTemplate` (single `{var}` matching exactly one
    /// non-empty segment), used to assert the catalog recognises live paths.
    fn template_matches(template: &str, path: &str) -> bool {
        match (template.find('{'), template.find('}')) {
            (Some(open), Some(close)) if close > open => {
                let prefix = &template[..open];
                let suffix = &template[close + 1..];
                if path.len() < prefix.len() + suffix.len()
                    || !path.starts_with(prefix)
                    || !path.ends_with(suffix)
                {
                    return false;
                }
                let var = &path[prefix.len()..path.len() - suffix.len()];
                !var.is_empty() && !var.contains('/') && !var.contains('.')
            }
            _ => template == path,
        }
    }

    fn catalog_recognises(cat: &[CatalogEntry], path: &str) -> bool {
        cat.iter().any(|e| template_matches(&e.template, path))
    }

    #[test]
    fn catalog_recognises_live_google_paths_and_still_flags_dead_schemes() {
        let cat = credential_catalog();
        for path in [
            "skills/google/oauth_client/client_id",
            "skills/google/oauth_client/client_secret",
            "skills/google/oauth_client/redirect_uri",
            "skills/google/default_workspace",
            "skills/google/workspaces/default/refresh_token",
            "skills/google/workspaces/default/access_token",
            "skills/google/workspaces/default/expires_at",
            "skills/google/workspaces/default/scopes",
            "skills/google/workspaces/default/email",
            "claude_code/oauth_token",
            "codex/auth_json",
        ] {
            assert!(
                catalog_recognises(&cat, path),
                "expected recognised: {path}"
            );
        }
        // Dead gog-CLI→Rust migration schemes must remain unrecognised.
        for path in ["google.gmail", "skills/google/oauth/token"] {
            assert!(
                !catalog_recognises(&cat, path),
                "expected still-unrecognised: {path}"
            );
        }
    }

    #[test]
    fn catalog_hides_runtime_oauth_material_but_recognises_it() {
        let cat = credential_catalog();
        let addable = |template: &str| {
            cat.iter()
                .find(|e| e.template == template)
                .unwrap_or_else(|| panic!("missing catalog entry: {template}"))
                .addable
        };
        assert!(addable(
            "skills/google/workspaces/{workspace}/refresh_token"
        ));
        assert!(!addable(
            "skills/google/workspaces/{workspace}/access_token"
        ));
        assert!(!addable("skills/google/workspaces/{workspace}/expires_at"));
        assert!(!addable("skills/google/workspaces/{workspace}/scopes"));
        assert!(!addable("skills/google/workspaces/{workspace}/email"));
        assert!(addable("skills/google/oauth_client/client_id"));
        // Per-skill default-workspace pointers are offerable (irregular suffixes too).
        assert!(addable("skills/google/default_workspace"));
        assert!(addable("skills/xero/default_entity"));
    }

    #[test]
    fn catalog_includes_source_tagged_mcp_entries() {
        let cat = credential_catalog();
        let find = |template: &str| {
            cat.iter()
                .find(|e| e.template == template)
                .unwrap_or_else(|| panic!("missing catalog entry: {template}"))
        };

        // GitHub MCP is PAT (API key) and offerable; built-in GitHub is separate.
        let gh = find("mcp/v1/github/token");
        assert_eq!(gh.source, "mcp");
        assert_eq!(gh.managed_by, "manual");
        assert!(gh.addable);

        // HubSpot MCP is OAuth: client creds offerable, tokens hidden.
        let hs = find("mcp/v1/hubspot/client_id");
        assert_eq!(hs.source, "mcp");
        assert_eq!(hs.managed_by, "oauth");
        assert!(hs.addable);
        assert!(!find("mcp/v1/hubspot/access_token").addable);

        // PostHog's shared-PAT fallback is an offerable, hand-entered key.
        let ph = find("mcp/v1/posthog/token");
        assert_eq!(ph.source, "mcp");
        assert_eq!(ph.managed_by, "manual");
        assert!(ph.addable);

        // Dynamic Metabase seeds no client creds and reuses the built-in
        // site_url — so it contributes no offerable MCP entry, only recognition.
        assert!(!cat
            .iter()
            .any(|e| e.source == "mcp" && e.skill.as_deref() == Some("metabase") && e.addable));
        assert!(catalog_recognises(&cat, "mcp/v1/metabase/access_token"));
        assert_eq!(
            cat.iter()
                .filter(|e| e.template == "skills/metabase/site_url")
                .count(),
            1,
            "metabase site_url must not be duplicated by the MCP mapping"
        );
    }

    #[test]
    fn every_catalog_template_resolves_to_a_concrete_path() {
        for e in credential_catalog() {
            let rep = representative_path(&e.template);
            assert!(!rep.contains('{') && !rep.contains('}'), "unfilled: {rep}");
            assert!(!rep.contains('*'), "wildcard leaked: {rep}");
            assert!(
                !rep.split(['/', '.']).any(|seg| seg.is_empty()),
                "empty segment: {rep}"
            );
            assert!(
                e.source == "builtin" || e.source == "mcp",
                "unexpected source `{}` for {}",
                e.source,
                e.template
            );
        }
    }

    // ─── Auto-load on restart (auto_load_configured_skills) ──────────────

    #[test]
    fn auto_load_loads_shared_credential_skill() {
        let vault = new_test_vault();
        vault.set("skills/linear/api_key", "lk_test").unwrap();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        auto_load_configured_skills(&vault, None, &config, &registry, &skill_state);

        assert!(
            skill_state.is_loaded("linear"),
            "shared-credential skill should be auto-loaded at startup"
        );
        assert!(
            registry.names().iter().any(|n| n.starts_with("linear.")),
            "expected linear tools registered after auto-load, got {:?}",
            registry.names()
        );
    }

    #[test]
    fn auto_load_skips_oauth_managed_skill() {
        // `google` is OAuth-managed: at boot there is no `CURRENT_USER_ID`, so
        // its per-user token can't resolve. Even with the shared oauth_client
        // present (which marks it "configured"), it must NOT auto-load.
        let dir = tempfile::tempdir().unwrap();
        let agent_path = dir.path().join("agent.enc");
        let global_path = dir.path().join("global.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let agent = Arc::new(Vault::open(&agent_path, &key).unwrap());
        let global = Arc::new(Vault::open(&global_path, &key).unwrap());
        global
            .set(
                "skills/google/oauth_client/client_id",
                "cid.apps.googleusercontent.com",
            )
            .unwrap();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        // google IS detected as configured…
        assert!(
            agent_configured_builtin_skills(Some(&agent), Some(&global))
                .iter()
                .any(|s| s == "google"),
            "precondition: google should count as configured"
        );
        // …but boot auto-load skips it (stays on-demand, resolved per-request).
        auto_load_configured_skills(&agent, Some(&global), &config, &registry, &skill_state);
        assert!(
            !skill_state.is_loaded("google"),
            "OAuth-managed google must stay on-demand at boot"
        );
    }

    #[test]
    fn auto_load_skips_obscura() {
        let vault = new_test_vault();
        vault.set("skills/obscura/token", "x").unwrap();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        auto_load_configured_skills(&vault, None, &config, &registry, &skill_state);
        assert!(
            !skill_state.is_loaded("obscura"),
            "obscura is opt-in only and must not auto-load"
        );
    }

    #[test]
    fn auto_load_is_idempotent_for_already_loaded_skill() {
        let vault = new_test_vault();
        vault.set("skills/linear/api_key", "lk_test").unwrap();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        // Pre-mark with a sentinel source; auto-load must skip (not re-load) it.
        skill_state.mark_loaded(
            "linear",
            vec!["linear.sentinel".to_string()],
            "manual",
            None,
        );
        auto_load_configured_skills(&vault, None, &config, &registry, &skill_state);

        let state = skill_state.get("linear").expect("linear still tracked");
        assert_eq!(
            state.source, "manual",
            "already-loaded skill must be left untouched (idempotent skip)"
        );
    }

    // ─── Registry-truth helpers (skill_tools_registered / load_and_track) ─

    struct NamedTool(String);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            &self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
        ) -> eyre::Result<workforce_agent::tools::ToolResult> {
            Ok(workforce_agent::tools::ToolResult::success(
                serde_json::Value::Null,
                0,
            ))
        }
    }

    #[test]
    fn skill_tools_registered_matches_the_catalog_prefix() {
        let registry = ToolRegistry::new();
        assert!(!skill_tools_registered("linear", &registry));

        registry.register(Arc::new(NamedTool("linear.issue_get".into())));
        assert!(skill_tools_registered("linear", &registry));
        assert!(!skill_tools_registered("jira", &registry));

        registry.register(Arc::new(NamedTool("record-keeper.apply_change".into())));
        assert!(skill_tools_registered("record-keeper", &registry));
    }

    #[test]
    fn skill_tools_registered_ignores_a_tracker_entry_without_tools() {
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        skill_state.mark_loaded("jira", vec!["jira.search".to_string()], "vault", None);

        assert!(skill_state.is_loaded("jira"));
        assert!(!skill_tools_registered("jira", &registry));
    }

    #[test]
    fn load_and_track_registers_tools_and_records_the_source() {
        let vault = new_test_vault();
        vault.set("skills/linear/api_key", "lk_test").unwrap();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        let result = load_and_track(
            "linear",
            Arc::clone(&vault),
            None,
            &config,
            &registry,
            &skill_state,
            "workflow-step",
        )
        .unwrap();

        assert!(!result.tool_names.is_empty());
        assert!(skill_tools_registered("linear", &registry));
        let state = skill_state.get("linear").expect("linear tracked");
        assert_eq!(state.source, "workflow-step");
        assert_eq!(state.tool_names, result.tool_names);
    }

    #[test]
    fn load_and_track_leaves_the_tracker_untouched_on_failure() {
        let vault = new_test_vault();
        let registry = ToolRegistry::new();
        let skill_state = crate::skill_state::SkillStateTracker::new();
        let config = WorkforceConfig::default();

        let err = load_and_track(
            "linear",
            vault,
            None,
            &config,
            &registry,
            &skill_state,
            "workflow-step",
        )
        .unwrap_err();

        assert!(err.to_string().contains("skills/linear"), "{err}");
        assert!(!skill_state.is_loaded("linear"));
        assert!(!skill_tools_registered("linear", &registry));
    }

    /// The prompt's `[ready]`/`[not configured]` badge now goes through this
    /// layered check. A per-user workspace cred must read as `ready` under the
    /// user's scope — the raw two-vault check the prompt used before missed it,
    /// mislabelling a configured skill "not configured" and making the agent
    /// walk the user through setup for something already set up.
    #[test]
    fn check_skill_credentials_sees_per_user_workspace_cred_when_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        vault
            .set("users/uid-1/skills/jira/workspaces/acme/api_token", "tok")
            .unwrap();

        let ready = |v: &Arc<Vault>| {
            check_skill_credentials(Arc::clone(v), None)
                .into_iter()
                .find(|s| s.name == "jira")
                .map(|s| s.ready)
                .unwrap_or(false)
        };

        let ready_scoped = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("uid-1".to_string(), || ready(&vault));
        assert!(
            ready_scoped,
            "per-user jira workspace cred should read as ready when identity scoped"
        );
        assert!(
            !ready(&vault),
            "the same per-user cred must not read as ready in an unscoped check"
        );
    }

    /// The "Your Connected Skills" prompt section lists per-user OAuth skills
    /// (google) that resolve as ready under the turn's identity scope. A user
    /// who connected google stores a refresh token at
    /// `users/{uid}/skills/google/workspaces/default/refresh_token`; that must
    /// read as ready ONLY when scoped to that uid (never in the shared/cached
    /// block, which is unscoped — that's why the cached badge is neutral).
    #[test]
    fn check_skill_credentials_sees_per_user_google_token_when_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        let key = Vault::resolve_master_key(None, dir.path()).unwrap();
        let vault = Arc::new(Vault::open(&path, &key).unwrap());
        vault
            .set(
                "users/uid-1/skills/google/workspaces/default/refresh_token",
                "rt",
            )
            .unwrap();

        let google_ready = |v: &Arc<Vault>| {
            check_skill_credentials(Arc::clone(v), None)
                .into_iter()
                .find(|s| s.name == "google")
                .map(|s| s.ready)
                .unwrap_or(false)
        };

        let ready_scoped = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope("uid-1".to_string(), || google_ready(&vault));
        assert!(
            ready_scoped,
            "per-user google refresh token should read as ready when identity scoped"
        );
        assert!(
            !google_ready(&vault),
            "per-user google token must not read as ready unscoped (shared/cached block)"
        );
    }
}
