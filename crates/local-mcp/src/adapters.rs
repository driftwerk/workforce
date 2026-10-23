//! Loading and dispatch for third-party adapter skills (Jira, Linear,
//! Datadog, Metabase, Postmark).
//!
//! Each adapter's `XxxSkill::new(config)` returns a set of
//! `workforce_agent::Tool`s. We build those at startup by reading
//! credentials from the [`LocalSecretStore`]
//! (`workforce-local-mcp config <adapter>` writes them), falling back to
//! legacy environment variables for CI / scripted use. The MCP tool
//! wrappers in `tools/adapters.rs` use `call` to invoke the right underlying
//! tool.
//!
//! ## Multi-workspace layout (see also `cli.rs`)
//!
//! Each adapter can hold N workspaces / accounts / instances. The on-disk
//! layout in the secret store is:
//!
//!   - `skills/<adapter>/workspaces_index`           — JSON array of names
//!   - `skills/<adapter>/workspaces/<name>/<field>`  — per-workspace fields
//!   - `skills/<adapter>/default_workspace`          — name for "no arg" calls
//!   - `skills/<adapter>/<field>`                    — legacy flat paths,
//!     read as an implicit `"default"` workspace if no explicit entry by
//!     that name exists in the index.
//!
//! The CLI labels everything `--workspace` for UX consistency, but the
//! underlying skill config structs name the map field per their own
//! conventions — `JiraSkillConfig.workspaces`, `DatadogSkillConfig.accounts`,
//! `MetabaseSkillConfig.instances`. This module bridges the two.

use std::collections::HashMap;
use std::sync::Arc;

use eyre::Result;
use serde_json::Value;
use tracing::{info, warn};
use workforce_agent::tools::Tool;
use workforce_common::settings::SecretStore;
use workforce_skills::adapters::datadog::config::DatadogAccountConfig;
use workforce_skills::adapters::datadog::{DatadogSkill, DatadogSkillConfig};
use workforce_skills::adapters::jira::config::JiraWorkspaceConfig;
use workforce_skills::adapters::jira::{JiraSkill, JiraSkillConfig};
use workforce_skills::adapters::linear::config::LinearWorkspaceConfig;
use workforce_skills::adapters::linear::{LinearSkill, LinearSkillConfig};
use workforce_skills::adapters::metabase::config::MetabaseInstanceConfig;
use workforce_skills::adapters::metabase::{MetabaseSkill, MetabaseSkillConfig};
use workforce_skills::adapters::postmark::config::PostmarkWorkspaceConfig;
use workforce_skills::adapters::postmark::{PostmarkSkill, PostmarkSkillConfig};
use workforce_skills::adapters::sendgrid::config::SendGridWorkspaceConfig;
use workforce_skills::adapters::sendgrid::{SendGridSkill, SendGridSkillConfig};
use workforce_skills::adapters::xero::config::XeroEntityConfig;
use workforce_skills::adapters::xero::{XeroSkill, XeroSkillConfig};

use crate::paths;
use crate::secret_store::LocalSecretStore;

const CONFIG_HINT: &str = "run `workforce-local-mcp config";
const LINEAR_BASE_URL: &str = "https://api.linear.app/graphql";
const DEFAULT_METABASE_ROW_CAP: u64 = 2000;

/// Minimal façade over a loaded skill: name → underlying tool.
pub struct AdapterHandle {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl AdapterHandle {
    fn from_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|t| (t.name().to_string(), t))
                .collect(),
        }
    }

    /// Iterate the underlying tools — used by the MCP layer to register each
    /// one as a first-class MCP tool at startup.
    pub fn tools(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.values()
    }

    /// Forward `input` to the named underlying tool and return its JSON
    /// output. Surfaces the adapter's `ToolResult.error` as a plain string
    /// so the MCP layer can bubble it straight back to the caller.
    pub async fn call(&self, name: &str, input: Value) -> Result<Value, String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| format!("tool '{name}' is not available"))?;
        let result = tool.execute(input).await.map_err(|e| e.to_string())?;
        if !result.success {
            return Err(result.error.unwrap_or_else(|| "tool failed".into()));
        }
        Ok(result.output)
    }
}

/// Bundle of optional adapter handles. Each field is `Some` only when at
/// least one workspace's required credentials resolved successfully.
pub struct Adapters {
    pub jira: Option<AdapterHandle>,
    pub linear: Option<AdapterHandle>,
    pub datadog: Option<AdapterHandle>,
    pub metabase: Option<AdapterHandle>,
    pub postmark: Option<AdapterHandle>,
    pub sendgrid: Option<AdapterHandle>,
    pub xero: Option<AdapterHandle>,
}

impl Adapters {
    /// Resolve each adapter from the keychain (preferred) or the legacy env
    /// vars (fallback). Missing or broken credentials log a warning and
    /// leave the adapter `None`; no startup errors are raised.
    pub fn load() -> Self {
        let store = LocalSecretStore::new();
        Self {
            jira: load_jira(&store),
            linear: load_linear(&store),
            datadog: load_datadog(&store),
            metabase: load_metabase(&store),
            postmark: load_postmark(&store),
            sendgrid: load_sendgrid(&store),
            xero: load_xero(&store),
        }
    }
}

// ── Shared loading primitives ───────────────────────────────────────────────

fn read_index(store: &dyn SecretStore, adapter: &str) -> Vec<String> {
    let Some(raw) = store.get(&paths::workspaces_index(adapter)) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<String>>(&raw) {
        Ok(list) => list,
        Err(e) => {
            warn!(adapter, error = %e, "workspaces_index is malformed — ignoring");
            Vec::new()
        }
    }
}

fn default_workspace_for(store: &dyn SecretStore, adapter: &str) -> Option<String> {
    store.get(&paths::default_workspace(adapter))
}

/// Log-once deprecation nudge when an env var fills in for a missing
/// keychain / file-store entry, pointing the user at the `config` CLI.
fn env_fallback(env: &str, path: &str) -> Option<String> {
    let v = std::env::var(env).ok()?;
    warn!("{env} fallback used for {path}; {CONFIG_HINT} <adapter> [--workspace NAME]` to migrate");
    Some(v)
}

/// Populate a `name → workspace config` map for an adapter by walking the
/// `workspaces_index` entries and, when the index doesn't already declare a
/// workspace named `"default"`, falling back to the adapter's legacy flat
/// paths. Returns `None` (with a one-line warning) when no workspaces
/// resolve — the caller treats that as "adapter disabled".
fn collect_workspaces<C>(
    store: &dyn SecretStore,
    adapter: &str,
    read_named: impl Fn(&dyn SecretStore, &str) -> Option<C>,
    read_legacy: impl FnOnce(&dyn SecretStore) -> Option<C>,
) -> Option<HashMap<String, C>> {
    let mut map: HashMap<String, C> = read_index(store, adapter)
        .into_iter()
        .filter_map(|name| read_named(store, &name).map(|cfg| (name, cfg)))
        .collect();

    if !map.contains_key("default") {
        if let Some(cfg) = read_legacy(store) {
            map.insert("default".to_string(), cfg);
        }
    }

    if map.is_empty() {
        warn!("{adapter}: no workspaces configured ({CONFIG_HINT} {adapter} [--workspace NAME]`)");
        return None;
    }
    Some(map)
}

// ── Jira ────────────────────────────────────────────────────────────────────

fn load_jira(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_jira_config(store)?;
    build("jira", || JiraSkill::new(cfg).map(|s| s.tools()))
}

fn build_jira_config(store: &dyn SecretStore) -> Option<JiraSkillConfig> {
    let workspaces = collect_workspaces(store, "jira", read_jira_workspace, read_jira_legacy)?;
    Some(JiraSkillConfig {
        email: None,
        api_token: None,
        site: None,
        default_project: None,
        workspaces,
        default_workspace: default_workspace_for(store, "jira"),
        prefix_map: HashMap::new(),
    })
}

fn read_jira_workspace(store: &dyn SecretStore, name: &str) -> Option<JiraWorkspaceConfig> {
    Some(JiraWorkspaceConfig {
        email: store.get(&paths::workspace_field("jira", name, "email"))?,
        api_token: store.get(&paths::workspace_field("jira", name, "api_token"))?,
        site: Some(store.get(&paths::workspace_field("jira", name, "site"))?),
        default_project: None,
        project_keys: Vec::new(),
    })
}

fn read_jira_legacy(store: &dyn SecretStore) -> Option<JiraWorkspaceConfig> {
    let field = |f: &str| paths::legacy_field("jira", f);
    Some(JiraWorkspaceConfig {
        email: store
            .get(&field("email"))
            .or_else(|| env_fallback("JIRA_EMAIL", &field("email")))?,
        api_token: store
            .get(&field("api_token"))
            .or_else(|| env_fallback("JIRA_API_TOKEN", &field("api_token")))?,
        site: Some(
            store
                .get(&field("site"))
                .or_else(|| env_fallback("JIRA_SITE", &field("site")))?,
        ),
        default_project: None,
        project_keys: Vec::new(),
    })
}

// ── Linear ──────────────────────────────────────────────────────────────────

fn load_linear(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_linear_config(store)?;
    build("linear", || LinearSkill::new(cfg).map(|s| s.tools()))
}

fn build_linear_config(store: &dyn SecretStore) -> Option<LinearSkillConfig> {
    let workspaces =
        collect_workspaces(store, "linear", read_linear_workspace, read_linear_legacy)?;
    Some(LinearSkillConfig {
        api_key: None,
        base_url: LINEAR_BASE_URL.to_string(),
        default_team_id: None,
        workspaces,
        default_workspace: default_workspace_for(store, "linear"),
        prefix_map: HashMap::new(),
    })
}

fn read_linear_workspace(store: &dyn SecretStore, name: &str) -> Option<LinearWorkspaceConfig> {
    Some(LinearWorkspaceConfig {
        api_key: store.get(&paths::workspace_field("linear", name, "api_key"))?,
        base_url: LINEAR_BASE_URL.to_string(),
        default_team_id: store.get(&paths::workspace_field("linear", name, "default_team_id")),
    })
}

fn read_linear_legacy(store: &dyn SecretStore) -> Option<LinearWorkspaceConfig> {
    let api_key_path = paths::legacy_field("linear", "api_key");
    Some(LinearWorkspaceConfig {
        api_key: store
            .get(&api_key_path)
            .or_else(|| env_fallback("LINEAR_API_KEY", &api_key_path))?,
        base_url: LINEAR_BASE_URL.to_string(),
        default_team_id: store
            .get(&paths::legacy_field("linear", "default_team_id"))
            .or_else(|| std::env::var("LINEAR_DEFAULT_TEAM_ID").ok()),
    })
}

// ── Datadog ─────────────────────────────────────────────────────────────────

fn load_datadog(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_datadog_config(store)?;
    build("datadog", || DatadogSkill::new(cfg).map(|s| s.tools()))
}

fn build_datadog_config(store: &dyn SecretStore) -> Option<DatadogSkillConfig> {
    // Datadog calls its entries "accounts" in the skill config; the CLI and
    // secret-store layout say "workspaces" for cross-adapter UX consistency.
    // We translate at the boundary.
    let accounts = collect_workspaces(
        store,
        "datadog",
        read_datadog_workspace,
        read_datadog_legacy,
    )?;
    Some(DatadogSkillConfig {
        api_key: None,
        app_key: None,
        site: None,
        base_url: None,
        accounts,
        default_account: default_workspace_for(store, "datadog"),
    })
}

fn read_datadog_workspace(store: &dyn SecretStore, name: &str) -> Option<DatadogAccountConfig> {
    Some(DatadogAccountConfig {
        api_key: store.get(&paths::workspace_field("datadog", name, "api_key"))?,
        app_key: store.get(&paths::workspace_field("datadog", name, "app_key"))?,
        site: store
            .get(&paths::workspace_field("datadog", name, "site"))
            .unwrap_or_else(|| "us1".to_string()),
        base_url: None,
    })
}

fn read_datadog_legacy(store: &dyn SecretStore) -> Option<DatadogAccountConfig> {
    let field = |f: &str| paths::legacy_field("datadog", f);
    Some(DatadogAccountConfig {
        api_key: store
            .get(&field("api_key"))
            .or_else(|| env_fallback("DATADOG_API_KEY", &field("api_key")))?,
        app_key: store
            .get(&field("app_key"))
            .or_else(|| env_fallback("DATADOG_APP_KEY", &field("app_key")))?,
        site: store
            .get(&field("site"))
            .or_else(|| std::env::var("DATADOG_SITE").ok())
            .unwrap_or_else(|| "us1".to_string()),
        base_url: None,
    })
}

// ── Metabase ────────────────────────────────────────────────────────────────

fn load_metabase(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_metabase_config(store)?;
    build("metabase", || MetabaseSkill::new(cfg).map(|s| s.tools()))
}

fn build_metabase_config(store: &dyn SecretStore) -> Option<MetabaseSkillConfig> {
    // Metabase calls its entries "instances" in the skill config; translate
    // from "workspaces" at the boundary, same pattern as Datadog.
    let instances = collect_workspaces(
        store,
        "metabase",
        read_metabase_workspace,
        read_metabase_legacy,
    )?;
    Some(MetabaseSkillConfig {
        api_key: None,
        site_url: None,
        max_query_rows: DEFAULT_METABASE_ROW_CAP,
        instances,
        default_instance: default_workspace_for(store, "metabase"),
    })
}

fn read_metabase_workspace(store: &dyn SecretStore, name: &str) -> Option<MetabaseInstanceConfig> {
    Some(MetabaseInstanceConfig {
        api_key: store.get(&paths::workspace_field("metabase", name, "api_key"))?,
        site_url: Some(store.get(&paths::workspace_field("metabase", name, "site_url"))?),
        max_query_rows: store
            .get(&paths::workspace_field("metabase", name, "max_query_rows"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_METABASE_ROW_CAP),
    })
}

fn read_metabase_legacy(store: &dyn SecretStore) -> Option<MetabaseInstanceConfig> {
    let field = |f: &str| paths::legacy_field("metabase", f);
    Some(MetabaseInstanceConfig {
        api_key: store
            .get(&field("api_key"))
            .or_else(|| env_fallback("METABASE_API_KEY", &field("api_key")))?,
        site_url: Some(
            store
                .get(&field("site_url"))
                .or_else(|| env_fallback("METABASE_URL", &field("site_url")))?,
        ),
        max_query_rows: store
            .get(&field("max_query_rows"))
            .or_else(|| std::env::var("METABASE_MAX_QUERY_ROWS").ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_METABASE_ROW_CAP),
    })
}

// ── Postmark ────────────────────────────────────────────────────────────────

fn load_postmark(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_postmark_config(store)?;
    build("postmark", || PostmarkSkill::new(cfg).map(|s| s.tools()))
}

fn build_postmark_config(store: &dyn SecretStore) -> Option<PostmarkSkillConfig> {
    let workspaces = collect_workspaces(
        store,
        "postmark",
        read_postmark_workspace,
        read_postmark_legacy,
    )?;
    Some(PostmarkSkillConfig {
        server_token: None,
        account_token: None,
        base_url: None,
        workspaces,
        default_workspace: default_workspace_for(store, "postmark"),
    })
}

fn read_postmark_workspace(store: &dyn SecretStore, name: &str) -> Option<PostmarkWorkspaceConfig> {
    Some(PostmarkWorkspaceConfig {
        server_token: store.get(&paths::workspace_field("postmark", name, "server_token"))?,
        account_token: store.get(&paths::workspace_field("postmark", name, "account_token")),
        base_url: store.get(&paths::workspace_field("postmark", name, "base_url")),
    })
}

fn read_postmark_legacy(store: &dyn SecretStore) -> Option<PostmarkWorkspaceConfig> {
    let field = |f: &str| paths::legacy_field("postmark", f);
    Some(PostmarkWorkspaceConfig {
        server_token: store
            .get(&field("server_token"))
            .or_else(|| env_fallback("POSTMARK_SERVER_TOKEN", &field("server_token")))?,
        account_token: store
            .get(&field("account_token"))
            .or_else(|| std::env::var("POSTMARK_ACCOUNT_TOKEN").ok()),
        base_url: store.get(&field("base_url")),
    })
}

// ── SendGrid ────────────────────────────────────────────────────────────────

fn load_sendgrid(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_sendgrid_config(store)?;
    build("sendgrid", || SendGridSkill::new(cfg).map(|s| s.tools()))
}

fn build_sendgrid_config(store: &dyn SecretStore) -> Option<SendGridSkillConfig> {
    let workspaces = collect_workspaces(
        store,
        "sendgrid",
        read_sendgrid_workspace,
        read_sendgrid_legacy,
    )?;
    Some(SendGridSkillConfig {
        workspaces,
        default_workspace: default_workspace_for(store, "sendgrid"),
        ..SendGridSkillConfig::default()
    })
}

fn read_sendgrid_workspace(store: &dyn SecretStore, name: &str) -> Option<SendGridWorkspaceConfig> {
    let field = |f: &str| store.get(&paths::workspace_field("sendgrid", name, f));
    Some(SendGridWorkspaceConfig {
        api_key: field("api_key")?,
        from_email: field("from_email"),
        from_name: field("from_name"),
        base_url: field("base_url"),
    })
}

fn read_sendgrid_legacy(store: &dyn SecretStore) -> Option<SendGridWorkspaceConfig> {
    let field = |f: &str| paths::legacy_field("sendgrid", f);
    Some(SendGridWorkspaceConfig {
        api_key: store
            .get(&field("api_key"))
            .or_else(|| env_fallback("SENDGRID_API_KEY", &field("api_key")))?,
        from_email: store.get(&field("from_email")),
        from_name: store.get(&field("from_name")),
        base_url: store.get(&field("base_url")),
    })
}

// ── Xero ──────────────────────────────────────────────────────────────────

fn load_xero(store: &dyn SecretStore) -> Option<AdapterHandle> {
    let cfg = build_xero_config(store)?;
    build("xero", || XeroSkill::new(cfg).map(|s| s.tools()))
}

fn build_xero_config(store: &dyn SecretStore) -> Option<XeroSkillConfig> {
    // Xero calls its entries "entities"; the CLI/store layout says "workspaces"
    // for cross-adapter UX consistency. Translate at the boundary.
    let entities = collect_workspaces(store, "xero", read_xero_workspace, read_xero_legacy)?;
    Some(XeroSkillConfig {
        client_id: None,
        client_secret: None,
        tenant_id: None,
        entities,
        default_entity: default_workspace_for(store, "xero"),
    })
}

fn read_xero_workspace(store: &dyn SecretStore, name: &str) -> Option<XeroEntityConfig> {
    Some(XeroEntityConfig {
        client_id: store.get(&paths::workspace_field("xero", name, "client_id"))?,
        client_secret: store.get(&paths::workspace_field("xero", name, "client_secret"))?,
        tenant_id: store.get(&paths::workspace_field("xero", name, "tenant_id")),
    })
}

fn read_xero_legacy(store: &dyn SecretStore) -> Option<XeroEntityConfig> {
    let field = |f: &str| paths::legacy_field("xero", f);
    Some(XeroEntityConfig {
        client_id: store
            .get(&field("client_id"))
            .or_else(|| env_fallback("XERO_CLIENT_ID", &field("client_id")))?,
        client_secret: store
            .get(&field("client_secret"))
            .or_else(|| env_fallback("XERO_CLIENT_SECRET", &field("client_secret")))?,
        tenant_id: store
            .get(&field("tenant_id"))
            .or_else(|| std::env::var("XERO_TENANT_ID").ok()),
    })
}

// ── Skill construction wrapper ──────────────────────────────────────────────

fn build(
    name: &'static str,
    f: impl FnOnce() -> Result<Vec<Arc<dyn Tool>>>,
) -> Option<AdapterHandle> {
    match f() {
        Ok(tools) => {
            info!(adapter = name, tools = tools.len(), "adapter enabled");
            Some(AdapterHandle::from_tools(tools))
        }
        Err(e) => {
            warn!(adapter = name, error = %e, "adapter init failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use workforce_agent::tools::ToolResult;

    // ── AdapterHandle tests (pre-existing behavior, unchanged) ──

    struct StubTool {
        name: &'static str,
        output: Value,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> Value {
            Value::Null
        }
        async fn execute(&self, _input: Value) -> Result<ToolResult> {
            Ok(ToolResult::success(self.output.clone(), 0))
        }
    }

    #[tokio::test]
    async fn call_forwards_to_named_tool() {
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(StubTool {
            name: "x.hello",
            output: serde_json::json!({"ok": true}),
        })];
        let h = AdapterHandle::from_tools(tools);
        let out = h.call("x.hello", Value::Null).await.unwrap();
        assert_eq!(out, serde_json::json!({"ok": true}));
    }

    #[tokio::test]
    async fn call_reports_missing_tool() {
        let h = AdapterHandle::from_tools(Vec::new());
        let err = h.call("nope", Value::Null).await.unwrap_err();
        assert!(err.contains("not available"));
    }

    // ── Stub SecretStore for config-builder tests ──
    //
    // Hermetic: holds a BTreeMap, implements only the trait methods we use
    // (`get`; set/delete/list are not exercised by build_*_config paths).

    struct MapStore {
        // Mutex so SecretStore's `Send + Sync` bound is satisfied — these
        // tests are single-threaded, the lock never contends.
        data: Mutex<BTreeMap<String, String>>,
    }

    impl MapStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(BTreeMap::new()),
            }
        }

        fn with(entries: &[(&str, &str)]) -> Self {
            let store = Self::new();
            for (k, v) in entries {
                store.insert(k, v);
            }
            store
        }

        fn insert(&self, path: &str, value: &str) {
            self.data.lock().unwrap().insert(path.into(), value.into());
        }
    }

    impl SecretStore for MapStore {
        fn get(&self, path: &str) -> Option<String> {
            self.data.lock().unwrap().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> Result<()> {
            self.data.lock().unwrap().insert(path.into(), value.into());
            Ok(())
        }
        fn delete(&self, path: &str) -> Result<bool> {
            Ok(self.data.lock().unwrap().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.data
                .lock()
                .unwrap()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    // ── build_linear_config behavior ──

    #[test]
    fn linear_config_empty_when_nothing_set() {
        let store = MapStore::new();
        assert!(build_linear_config(&store).is_none());
    }

    #[test]
    fn linear_config_legacy_flat_becomes_default_workspace() {
        let store = MapStore::with(&[("skills/linear/api_key", "lin_legacy")]);
        let cfg = build_linear_config(&store).expect("legacy creds should resolve");
        assert!(
            cfg.api_key.is_none(),
            "flat api_key should not be set on the outer struct"
        );
        assert_eq!(cfg.workspaces.len(), 1);
        let default = cfg
            .workspaces
            .get("default")
            .expect("default workspace populated from legacy");
        assert_eq!(default.api_key, "lin_legacy");
    }

    #[test]
    fn linear_config_named_workspaces_from_index() {
        let store = MapStore::with(&[
            ("skills/linear/workspaces_index", r#"["work","personal"]"#),
            ("skills/linear/workspaces/work/api_key", "lin_work"),
            ("skills/linear/workspaces/work/default_team_id", "TEAM-1"),
            ("skills/linear/workspaces/personal/api_key", "lin_personal"),
        ]);
        let cfg = build_linear_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(cfg.workspaces["work"].api_key, "lin_work");
        assert_eq!(
            cfg.workspaces["work"].default_team_id.as_deref(),
            Some("TEAM-1")
        );
        assert_eq!(cfg.workspaces["personal"].api_key, "lin_personal");
        assert!(cfg.workspaces["personal"].default_team_id.is_none());
    }

    #[test]
    fn linear_config_explicit_default_wins_over_legacy() {
        // When the index already lists "default", the legacy flat paths
        // must NOT silently override the explicit workspace.
        let store = MapStore::with(&[
            ("skills/linear/workspaces_index", r#"["default"]"#),
            ("skills/linear/workspaces/default/api_key", "lin_explicit"),
            ("skills/linear/api_key", "lin_legacy"),
        ]);
        let cfg = build_linear_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces["default"].api_key, "lin_explicit");
    }

    #[test]
    fn linear_config_carries_default_workspace_pointer() {
        let store = MapStore::with(&[
            ("skills/linear/workspaces_index", r#"["work"]"#),
            ("skills/linear/workspaces/work/api_key", "lin_work"),
            ("skills/linear/default_workspace", "work"),
        ]);
        let cfg = build_linear_config(&store).unwrap();
        assert_eq!(cfg.default_workspace.as_deref(), Some("work"));
    }

    // ── build_jira_config spot-check ──

    #[test]
    fn jira_config_named_workspace_requires_all_three_fields() {
        // Missing api_token → workspace drops, not partial.
        let store = MapStore::with(&[
            ("skills/jira/workspaces_index", r#"["acme"]"#),
            ("skills/jira/workspaces/acme/email", "me@acme.com"),
            (
                "skills/jira/workspaces/acme/site",
                "https://acme.atlassian.net",
            ),
        ]);
        assert!(build_jira_config(&store).is_none());

        let store = MapStore::with(&[
            ("skills/jira/workspaces_index", r#"["acme"]"#),
            ("skills/jira/workspaces/acme/email", "me@acme.com"),
            (
                "skills/jira/workspaces/acme/site",
                "https://acme.atlassian.net",
            ),
            ("skills/jira/workspaces/acme/api_token", "tok"),
        ]);
        let cfg = build_jira_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(
            cfg.workspaces["acme"].site.as_deref(),
            Some("https://acme.atlassian.net")
        );
    }

    // ── build_datadog_config / build_metabase_config: map renaming ──

    #[test]
    fn datadog_workspace_lands_in_accounts_map() {
        let store = MapStore::with(&[
            ("skills/datadog/workspaces_index", r#"["prod"]"#),
            ("skills/datadog/workspaces/prod/api_key", "dd_api"),
            ("skills/datadog/workspaces/prod/app_key", "dd_app"),
            ("skills/datadog/workspaces/prod/site", "eu1"),
            ("skills/datadog/default_workspace", "prod"),
        ]);
        let cfg = build_datadog_config(&store).unwrap();
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts["prod"].api_key, "dd_api");
        assert_eq!(cfg.accounts["prod"].site, "eu1");
        assert_eq!(cfg.default_account.as_deref(), Some("prod"));
    }

    #[test]
    fn metabase_workspace_lands_in_instances_map() {
        let store = MapStore::with(&[
            ("skills/metabase/workspaces_index", r#"["staging"]"#),
            ("skills/metabase/workspaces/staging/api_key", "mb_key"),
            (
                "skills/metabase/workspaces/staging/site_url",
                "https://mb-staging.example.com",
            ),
        ]);
        let cfg = build_metabase_config(&store).unwrap();
        assert_eq!(cfg.instances.len(), 1);
        assert_eq!(
            cfg.instances["staging"].site_url.as_deref(),
            Some("https://mb-staging.example.com")
        );
        assert_eq!(
            cfg.instances["staging"].max_query_rows,
            DEFAULT_METABASE_ROW_CAP
        );
    }

    // ── build_xero_config: map renaming + required fields ──

    #[test]
    fn xero_workspace_lands_in_entities_map() {
        let store = MapStore::with(&[
            ("skills/xero/workspaces_index", r#"["harbor"]"#),
            ("skills/xero/workspaces/harbor/client_id", "cid"),
            ("skills/xero/workspaces/harbor/client_secret", "secret"),
            ("skills/xero/default_workspace", "harbor"),
        ]);
        let cfg = build_xero_config(&store).unwrap();
        assert_eq!(cfg.entities.len(), 1);
        assert_eq!(cfg.entities["harbor"].client_id, "cid");
        assert!(cfg.entities["harbor"].tenant_id.is_none());
        assert_eq!(cfg.default_entity.as_deref(), Some("harbor"));
    }

    #[test]
    fn xero_workspace_requires_client_secret() {
        let store = MapStore::with(&[
            ("skills/xero/workspaces_index", r#"["beacon"]"#),
            ("skills/xero/workspaces/beacon/client_id", "cid"),
        ]);
        assert!(build_xero_config(&store).is_none());
    }

    // ── build_sendgrid_config behavior ──

    #[test]
    fn sendgrid_config_empty_when_nothing_set() {
        assert!(build_sendgrid_config(&MapStore::new()).is_none());
    }

    #[test]
    fn sendgrid_legacy_flat_becomes_default_workspace() {
        let store = MapStore::with(&[
            ("skills/sendgrid/api_key", "SG.legacy"),
            ("skills/sendgrid/from_email", "noreply@example.com"),
        ]);
        let cfg = build_sendgrid_config(&store).expect("legacy creds should resolve");
        assert!(cfg.api_key.is_none(), "flat field not on outer struct");
        let default = cfg.workspaces.get("default").expect("default synthesised");
        assert_eq!(default.api_key, "SG.legacy");
        assert_eq!(default.from_email.as_deref(), Some("noreply@example.com"));
        assert!(default.from_name.is_none());
    }

    #[test]
    fn sendgrid_named_workspace_requires_api_key() {
        let store = MapStore::with(&[
            ("skills/sendgrid/workspaces_index", r#"["broken"]"#),
            (
                "skills/sendgrid/workspaces/broken/from_email",
                "x@example.com",
            ),
        ]);
        assert!(build_sendgrid_config(&store).is_none());
    }

    #[test]
    fn sendgrid_workspace_carries_sender_and_default() {
        let store = MapStore::with(&[
            ("skills/sendgrid/workspaces_index", r#"["work","eu"]"#),
            ("skills/sendgrid/workspaces/work/api_key", "SG.work"),
            ("skills/sendgrid/workspaces/work/from_name", "Work"),
            ("skills/sendgrid/workspaces/eu/api_key", "SG.eu"),
            (
                "skills/sendgrid/workspaces/eu/base_url",
                "https://api.eu.sendgrid.com",
            ),
            ("skills/sendgrid/default_workspace", "eu"),
        ]);
        let cfg = build_sendgrid_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(cfg.workspaces["work"].from_name.as_deref(), Some("Work"));
        assert_eq!(
            cfg.workspaces["eu"].base_url.as_deref(),
            Some("https://api.eu.sendgrid.com")
        );
        assert_eq!(cfg.default_workspace.as_deref(), Some("eu"));
    }

    // ── build_postmark_config behavior ──

    #[test]
    fn postmark_config_empty_when_nothing_set() {
        let store = MapStore::new();
        assert!(build_postmark_config(&store).is_none());
    }

    #[test]
    fn postmark_legacy_flat_becomes_default_workspace() {
        // Only server_token set; account_token is optional.
        let store = MapStore::with(&[("skills/postmark/server_token", "pm_legacy")]);
        let cfg = build_postmark_config(&store).expect("legacy creds should resolve");
        assert!(cfg.server_token.is_none(), "flat field not on outer struct");
        assert_eq!(cfg.workspaces.len(), 1);
        let default = cfg.workspaces.get("default").expect("default synthesised");
        assert_eq!(default.server_token, "pm_legacy");
        assert!(default.account_token.is_none());
    }

    #[test]
    fn postmark_named_workspace_without_account_token_still_resolves() {
        // Graceful degradation: server_token alone is enough to load the
        // skill; account-scoped tools surface a runtime error at call time.
        let store = MapStore::with(&[
            ("skills/postmark/workspaces_index", r#"["prod"]"#),
            ("skills/postmark/workspaces/prod/server_token", "pm_prod"),
        ]);
        let cfg = build_postmark_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 1);
        assert_eq!(cfg.workspaces["prod"].server_token, "pm_prod");
        assert!(cfg.workspaces["prod"].account_token.is_none());
    }

    #[test]
    fn postmark_named_workspace_requires_server_token() {
        // account_token alone should NOT produce a workspace — the workspace
        // is dropped because its required field is missing.
        let store = MapStore::with(&[
            ("skills/postmark/workspaces_index", r#"["broken"]"#),
            ("skills/postmark/workspaces/broken/account_token", "pm_acct"),
        ]);
        assert!(build_postmark_config(&store).is_none());
    }

    #[test]
    fn postmark_workspace_carries_account_token_and_default() {
        let store = MapStore::with(&[
            ("skills/postmark/workspaces_index", r#"["work","personal"]"#),
            (
                "skills/postmark/workspaces/work/server_token",
                "pm_work_srv",
            ),
            (
                "skills/postmark/workspaces/work/account_token",
                "pm_work_acct",
            ),
            (
                "skills/postmark/workspaces/personal/server_token",
                "pm_personal_srv",
            ),
            ("skills/postmark/default_workspace", "work"),
        ]);
        let cfg = build_postmark_config(&store).unwrap();
        assert_eq!(cfg.workspaces.len(), 2);
        assert_eq!(
            cfg.workspaces["work"].account_token.as_deref(),
            Some("pm_work_acct")
        );
        assert!(cfg.workspaces["personal"].account_token.is_none());
        assert_eq!(cfg.default_workspace.as_deref(), Some("work"));
    }
}
