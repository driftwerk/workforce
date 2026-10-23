//! Global config overlay.
//!
//! `workforce.toml` is deployed read-only from the infra repo — the next
//! deploy overwrites it, so it can never hold runtime changes. Runtime config
//! changes land in `{data_dir}/workforce.overlay.toml` instead, which lives on
//! the persistent volume (survives deploys) and is merged over the base at
//! load time at the **raw `toml::Value` level** — before env expansion — so
//! `${VAR}` / vault references stay unexpanded and the promote/export flow
//! never serializes resolved secrets.
//!
//! The overlay is **deny-by-default**: the read-only mount was an inadvertent
//! security control (the only thing stopping a prompt-injected agent from
//! rewriting `[postgres]`, providers, or auth via the full-config writers),
//! and [`OVERLAYABLE_PREFIXES`] restores that boundary deliberately. The
//! writer refuses non-allowlisted keys with a typed error; the loader strips
//! them with an error-level log as defense in depth against hand-edits.

use std::path::{Path, PathBuf};

/// Overlay filename, resolved against the base config's `hub.data_dir`.
pub const OVERLAY_FILE: &str = "workforce.overlay.toml";

/// Key-path prefixes a runtime overlay may set. Everything else —
/// `[postgres]`, `[gateway]`, `[vault]`, `[hub]` (ports/data dirs), provider
/// endpoints/keys, auth, and the `[permissions]` / `[approvals]` security
/// guardrails — is infra-owned and can only change in the base (disabling
/// authz or approvals must be a deploy, never a runtime toggle).
///
/// Two trust tiers share this list. The admin/owner write path (the
/// management UI's `PUT /api/config/overlay`) and the loader accept the full
/// set below. Agent-reachable writers are confined further to
/// [`AGENT_OVERLAYABLE_KEYS`] (see [`ensure_agent_entry_scope`]) — so widening
/// this list with inert feature toggles can never hand a prompt-injected agent
/// a security knob: the guardrails and infra sections are absent here, and an
/// agent can only ever touch agent entries regardless.
///
/// `mcp` is intentionally narrowed to `mcp.enabled`: `[mcp]` also carries an
/// `auth_token` (a secret), a `listen` bind address, and a `servers` map,
/// which stay infra-owned (manage servers from the MCP Servers tab).
pub const OVERLAYABLE_PREFIXES: &[&str] = &[
    "agents",
    "agents_remove",
    "llm.routing",
    "llm.model_preferences",
    "llm.equivalences",
    "mcp.enabled",
    "environments",
    "games",
];

/// The overlay keys an *agent-reachable* writer may change. Agent tools create,
/// update, and delete agent entries — nothing else. The admin/owner write path
/// may set the wider [`OVERLAYABLE_PREFIXES`]; this is the writer-side half of
/// that trust boundary (the loader's half is bounded by `OVERLAYABLE_PREFIXES`
/// via [`strip_offending`]).
pub const AGENT_OVERLAYABLE_KEYS: &[&str] = &["agents", "agents_remove"];

/// Refuse an agent-tier overlay mutation that touched anything outside
/// [`AGENT_OVERLAYABLE_KEYS`]. `before` / `after` are the overlay table either
/// side of an agent writer's mutation; everything but the agent keys must be
/// identical, so a future agent tool reaching the generic writer still could
/// not flip an operator feature flag.
pub fn ensure_agent_entry_scope(before: &toml::Table, after: &toml::Table) -> eyre::Result<()> {
    let without_agent_keys = |t: &toml::Table| {
        let mut t = t.clone();
        for key in AGENT_OVERLAYABLE_KEYS {
            t.remove(*key);
        }
        t
    };
    if without_agent_keys(before) != without_agent_keys(after) {
        eyre::bail!(
            "agent-tier overlay write may only change agent entries \
             (agents / agents_remove)"
        );
    }
    Ok(())
}

/// Carry the agent-owned keys ([`AGENT_OVERLAYABLE_KEYS`]) from `live` into
/// `target`, overwriting whatever `target` held. The full-overlay replace
/// behind `PUT /api/config/overlay` must not own agent entries — the surgical
/// agent writers do (they enforce the default/last-agent removal guard and
/// avoid lost updates against a concurrent create). So the admin editor edits
/// config keys while the live agent set is preserved verbatim.
pub fn preserve_agent_keys(target: &mut toml::Table, live: &toml::Table) {
    for key in AGENT_OVERLAYABLE_KEYS {
        match live.get(*key) {
            Some(value) => {
                target.insert((*key).to_string(), value.clone());
            }
            None => {
                target.remove(*key);
            }
        }
    }
}

/// `true` when `path` is covered by an allowlisted prefix (the prefix itself
/// or anything beneath it).
fn path_allowed(path: &str) -> bool {
    OVERLAYABLE_PREFIXES
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}.")))
}

/// `true` when `path` is a strict ancestor of an allowlisted prefix (e.g.
/// `llm` on the way to `llm.routing`) — descend instead of rejecting.
fn path_opens_allowed(path: &str) -> bool {
    OVERLAYABLE_PREFIXES
        .iter()
        .any(|p| p.starts_with(&format!("{path}.")))
}

fn collect_offending(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        if path_allowed(&path) {
            continue;
        }
        if let (true, Some(sub)) = (path_opens_allowed(&path), value.as_table()) {
            collect_offending(sub, &path, out);
            continue;
        }
        out.push(path);
    }
}

/// Key paths in `table` that the allowlist rejects (empty = valid overlay).
pub fn offending_paths(table: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    collect_offending(table, "", &mut out);
    out
}

/// Remove every non-allowlisted key path, returning what was stripped.
/// Load-time defense in depth — the writer is the real boundary.
pub fn strip_offending(table: &mut toml::Table) -> Vec<String> {
    let offending = offending_paths(table);
    for path in &offending {
        remove_path(table, path);
    }
    offending
}

fn remove_path(table: &mut toml::Table, path: &str) {
    // A literal quoted key (`"postgres.enabled" = …`) matches the full path
    // as one table key — remove it directly before descending dotted paths.
    if table.remove(path).is_some() {
        return;
    }
    if let Some((head, rest)) = path.split_once('.') {
        if let Some(sub) = table.get_mut(head).and_then(|v| v.as_table_mut()) {
            remove_path(sub, rest);
            if sub.is_empty() {
                table.remove(head);
            }
        }
    }
}

/// Resolve the overlay path from the (raw, pre-merge) base table: env-expand
/// `hub.data_dir` (it may be `${VAR}`-valued) and join [`OVERLAY_FILE`].
pub fn overlay_path(base_table: &toml::Table) -> PathBuf {
    let data_dir = base_table
        .get("hub")
        .and_then(|h| h.get("data_dir"))
        .and_then(|v| v.as_str())
        .map(crate::config_expand::expand_env_vars)
        .unwrap_or_else(|| "./data".to_string());
    Path::new(&data_dir).join(OVERLAY_FILE)
}

/// Deep-merge `overlay` into `base` (overlay wins). `[[agents]]` merges **by
/// id**: a new id appends, an existing id replaces that agent's entry
/// wholesale (per-entity replace, not field-merge). A top-level
/// `agents_remove = ["id", …]` list drops those agents from the merged set
/// and is consumed (it is not a `WorkforceConfig` field).
pub fn merge_overlay(base: &mut toml::Table, overlay: &toml::Table) {
    for (key, value) in overlay {
        match key.as_str() {
            "agents" => merge_agents(base, value),
            "agents_remove" => {} // applied below, after the agent merge
            _ => match base.get_mut(key) {
                Some(existing) => deep_merge(existing, value),
                None => {
                    base.insert(key.clone(), value.clone());
                }
            },
        }
    }
    if let Some(remove) = overlay.get("agents_remove").and_then(|v| v.as_array()) {
        let ids: Vec<&str> = remove.iter().filter_map(|v| v.as_str()).collect();
        if let Some(agents) = base.get_mut("agents").and_then(|v| v.as_array_mut()) {
            agents.retain(|a| {
                a.get("id")
                    .and_then(|i| i.as_str())
                    .is_none_or(|id| !ids.contains(&id))
            });
        }
    }
}

fn merge_agents(base: &mut toml::Table, overlay_agents: &toml::Value) {
    let Some(items) = overlay_agents.as_array() else {
        return;
    };
    let entry = base
        .entry("agents".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let Some(arr) = entry.as_array_mut() else {
        return;
    };
    for item in items {
        let id = item.get("id").and_then(|v| v.as_str());
        let pos = id.and_then(|id| {
            arr.iter()
                .position(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
        });
        match pos {
            Some(i) => arr[i] = item.clone(),
            None => arr.push(item.clone()),
        }
    }
}

fn deep_merge(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(bv) => deep_merge(bv, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

/// Base + overlay raw tables, as read from disk. The single load shape shared
/// by boot ([`crate::settings::WorkforceConfig::load`]) and the overlay
/// writer's pre-write validation, so write-accepts ⇔ boot-accepts.
pub struct RawConfig {
    pub base: toml::Table,
    /// Parsed overlay (`None` when the file is absent or unparsable).
    pub overlay: Option<toml::Table>,
    pub overlay_path: PathBuf,
    /// Non-allowlisted key paths stripped from the overlay (hand-edits).
    pub stripped: Vec<String>,
    /// Overlay read/parse failure. The hub must still boot — base-only —
    /// but callers log this at error level: any overlay-defined agents
    /// vanish until the file is fixed.
    pub overlay_error: Option<String>,
}

impl RawConfig {
    /// The merged (base ⊕ overlay) raw table.
    pub fn merged(&self) -> toml::Table {
        let mut merged = self.base.clone();
        if let Some(overlay) = &self.overlay {
            merge_overlay(&mut merged, overlay);
        }
        merged
    }
}

/// The key paths an overlay sets, in display form: one `agents.<id>` per
/// agent entry, `agents_remove.<id>` per tombstone, and deep scalar paths
/// for everything else. Drives the provenance view and the revert action.
pub fn overlay_key_paths(overlay: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in overlay {
        match (key.as_str(), value) {
            ("agents", toml::Value::Array(items)) => {
                for item in items {
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        out.push(format!("agents.{id}"));
                    }
                }
            }
            ("agents_remove", toml::Value::Array(ids)) => {
                for id in ids.iter().filter_map(|v| v.as_str()) {
                    out.push(format!("agents_remove.{id}"));
                }
            }
            (_, toml::Value::Table(sub)) => collect_leaf_paths(sub, key, &mut out),
            _ => out.push(key.clone()),
        }
    }
    out
}

fn collect_leaf_paths(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = format!("{prefix}.{key}");
        match value {
            toml::Value::Table(sub) => collect_leaf_paths(sub, &path, out),
            _ => out.push(path),
        }
    }
}

fn value_at_path<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    match path.split_once('.') {
        None => table.get(path),
        Some((head, rest)) => value_at_path(table.get(head)?.as_table()?, rest),
    }
}

/// `true` when an overlay key has **converged** with the base — the override
/// is now redundant (an infra deploy caught up) and can be cleared. This is
/// the guided exit from the sticky-override footgun: overlay-wins means a
/// base change can never take effect under a live override, so the UI flags
/// converged keys for one-click removal.
pub fn key_path_converged(base: &toml::Table, overlay: &toml::Table, key_path: &str) -> bool {
    if let Some(id) = key_path.strip_prefix("agents.") {
        let find = |t: &toml::Table| -> Option<toml::Value> {
            t.get("agents")?
                .as_array()?
                .iter()
                .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
                .cloned()
        };
        return match (find(base), find(overlay)) {
            (Some(b), Some(o)) => b == o,
            _ => false,
        };
    }
    if let Some(id) = key_path.strip_prefix("agents_remove.") {
        // A tombstone converges once the base no longer defines the agent.
        return !base
            .get("agents")
            .and_then(|v| v.as_array())
            .is_some_and(|arr| {
                arr.iter()
                    .any(|a| a.get("id").and_then(|v| v.as_str()) == Some(id))
            });
    }
    match (
        value_at_path(base, key_path),
        value_at_path(overlay, key_path),
    ) {
        (Some(b), Some(o)) => b == o,
        _ => false,
    }
}

/// Remove one overlay key path (revert-to-base). `agents.<id>` drops that
/// agent entry; `agents_remove.<id>` un-tombstones the id; anything else is
/// a deep key removal. Returns `false` when the path wasn't present.
pub fn remove_overlay_path(overlay: &mut toml::Table, key_path: &str) -> bool {
    if let Some(id) = key_path.strip_prefix("agents.") {
        let Some(arr) = overlay.get_mut("agents").and_then(|v| v.as_array_mut()) else {
            return false;
        };
        let before = arr.len();
        arr.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(id));
        let removed = arr.len() != before;
        if arr.is_empty() {
            overlay.remove("agents");
        }
        return removed;
    }
    if let Some(id) = key_path.strip_prefix("agents_remove.") {
        let Some(arr) = overlay
            .get_mut("agents_remove")
            .and_then(|v| v.as_array_mut())
        else {
            return false;
        };
        let before = arr.len();
        arr.retain(|v| v.as_str() != Some(id));
        let removed = arr.len() != before;
        if arr.is_empty() {
            overlay.remove("agents_remove");
        }
        return removed;
    }
    if value_at_path(overlay, key_path).is_none() {
        return false;
    }
    remove_path(overlay, key_path);
    true
}

/// Read the base config and its overlay as raw tables (no env expansion).
pub fn load_raw(base_path: &Path) -> eyre::Result<RawConfig> {
    let content = std::fs::read_to_string(base_path).map_err(|e| {
        eyre::eyre!(
            "Failed to read config file '{}': {}",
            base_path.display(),
            e
        )
    })?;
    let base: toml::Table = content
        .parse()
        .map_err(|e| eyre::eyre!("Failed to parse config: {}", e))?;

    let overlay_path = overlay_path(&base);
    let (overlay, stripped, overlay_error) = match crate::config_files::read(&overlay_path) {
        Ok(Some(raw)) => match raw.parse::<toml::Table>() {
            Ok(mut table) => {
                let stripped = strip_offending(&mut table);
                (Some(table), stripped, None)
            }
            Err(e) => (None, Vec::new(), Some(e.to_string())),
        },
        Ok(None) => (None, Vec::new(), None),
        Err(e) => (None, Vec::new(), Some(e.to_string())),
    };

    Ok(RawConfig {
        base,
        overlay,
        overlay_path,
        stripped,
        overlay_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Table {
        s.parse().expect("test toml parses")
    }

    // ── Allowlist: the security boundary ─────────────────────────

    #[test]
    fn allowlist_rejects_infra_owned_sections() {
        for bad in [
            "[postgres]\nenabled = false\n",
            "[gateway]\nauth_token = \"x\"\n",
            "[vault]\npath = \"/etc\"\n",
            "[hub]\ndata_dir = \"/tmp/steal\"\n",
            "[llm]\napi_key = \"sk-stolen\"\n",
            "[llm]\nprovider = \"evil\"\n",
            "[identity]\nbackend = \"sqlite\"\n",
        ] {
            let offending = offending_paths(&t(bad));
            assert!(
                !offending.is_empty(),
                "must reject overlay: {bad} (got no offending paths)"
            );
        }
    }

    #[test]
    fn allowlist_accepts_runtime_sections() {
        let ok = t("[[agents]]\nid = \"maple\"\nname = \"Maple\"\n\
                    [llm.routing]\nenabled = true\n\
                    [llm.model_preferences]\ncoding = \"opus\"\n\
                    [llm.equivalences]\nfast = [\"a\", \"b\"]\n");
        assert!(offending_paths(&ok).is_empty());
        let removes: toml::Table = t("agents_remove = [\"old\"]\n");
        assert!(offending_paths(&removes).is_empty());
    }

    #[test]
    fn allowlist_rejects_llm_siblings_while_allowing_routing() {
        // `llm` opens the path to llm.routing but its other children stay shut.
        let mixed = t("[llm]\napi_key = \"sk-x\"\n[llm.routing]\nenabled = true\n");
        let offending = offending_paths(&mixed);
        assert_eq!(offending, vec!["llm.api_key".to_string()]);
    }

    #[test]
    fn strip_offending_removes_only_bad_paths() {
        let mut mixed = t("[llm]\napi_key = \"sk-x\"\n[llm.routing]\nenabled = true\n\
                           [postgres]\nenabled = false\n");
        let stripped = strip_offending(&mut mixed);
        assert_eq!(stripped.len(), 2);
        assert!(mixed.get("postgres").is_none());
        let llm = mixed.get("llm").and_then(|v| v.as_table()).unwrap();
        assert!(llm.get("api_key").is_none());
        assert!(llm.get("routing").is_some());
    }

    #[test]
    fn strip_offending_handles_literal_dotted_keys() {
        // A quoted literal key is a single table key whose NAME contains
        // dots — it must be rejected AND actually removed.
        let mut sneaky = t("\"postgres.enabled\" = true\n[[agents]]\nid = \"ok\"\n");
        let stripped = strip_offending(&mut sneaky);
        assert_eq!(stripped, vec!["postgres.enabled".to_string()]);
        assert!(sneaky.get("postgres.enabled").is_none());
        assert!(sneaky.get("agents").is_some());
    }

    #[test]
    fn allowlist_accepts_operator_feature_toggles() {
        let ok = t("[mcp]\nenabled = true\n\
                    [environments]\nenabled = true\n\
                    [games]\nenabled = true\nagent_players = [\"atlas\"]\n");
        assert!(offending_paths(&ok).is_empty());
    }

    #[test]
    fn allowlist_rejects_mcp_secrets_while_allowing_enabled() {
        // mcp.enabled is overlayable; the secret/bind/server keys are not.
        let mixed = t("[mcp]\nenabled = true\nauth_token = \"sk-x\"\n\
                       listen = \"0.0.0.0:9\"\n");
        let mut offending = offending_paths(&mixed);
        offending.sort();
        assert_eq!(offending, vec!["mcp.auth_token", "mcp.listen"]);
    }

    #[test]
    fn allowlist_keeps_security_guardrails_infra_owned() {
        for bad in [
            "[permissions]\nenabled = false\n",
            "[approvals]\nenabled = false\n",
        ] {
            assert!(!offending_paths(&t(bad)).is_empty(), "must reject: {bad}");
        }
    }

    /// `[budget]` is the mandatory-spend guardrail: no runtime writer may
    /// loosen it. It must never join `OVERLAYABLE_PREFIXES`.
    #[test]
    fn allowlist_keeps_budget_config_file_only() {
        for bad in [
            "[budget]\nenabled = false\n",
            "[budget]\nhub_daily_usd = 999999.0\n",
            "[budget.agents.atlas]\nagent_daily_usd = 999999.0\n",
        ] {
            assert!(!offending_paths(&t(bad)).is_empty(), "must reject: {bad}");
        }
    }

    #[test]
    fn agent_entry_scope_allows_agent_only_changes() {
        let before = t("[mcp]\nenabled = true\n[[agents]]\nid = \"maple\"\n");
        let after = t("[mcp]\nenabled = true\n\
                       [[agents]]\nid = \"maple\"\n[[agents]]\nid = \"allan\"\n");
        assert!(ensure_agent_entry_scope(&before, &after).is_ok());
    }

    #[test]
    fn preserve_agent_keys_takes_live_agents_over_submission() {
        // The admin's text box tried to inject an agent and tombstone the
        // default; both must be discarded in favour of the live agent set,
        // while the genuine config edit (mcp.enabled) is kept.
        let mut submitted = t("agents_remove = [\"default\"]\n[mcp]\nenabled = true\n\
                               [[agents]]\nid = \"evil\"\n");
        let live = t("[[agents]]\nid = \"maple\"\n");
        preserve_agent_keys(&mut submitted, &live);
        assert_eq!(
            submitted.get("mcp").and_then(|m| m.get("enabled")),
            Some(&toml::Value::Boolean(true))
        );
        let agents = submitted.get("agents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].get("id").and_then(|v| v.as_str()), Some("maple"));
        assert!(submitted.get("agents_remove").is_none());
    }

    #[test]
    fn agent_entry_scope_rejects_touching_other_keys() {
        // An agent writer that somehow flipped an operator toggle is refused,
        // even though mcp.enabled is admin-overlayable.
        let before = t("[mcp]\nenabled = true\n[[agents]]\nid = \"maple\"\n");
        let after = t("[mcp]\nenabled = false\n[[agents]]\nid = \"maple\"\n");
        assert!(ensure_agent_entry_scope(&before, &after).is_err());
    }

    // ── Merge semantics ──────────────────────────────────────────

    #[test]
    fn agents_merge_by_id_replace_and_append() {
        let mut base = t("[[agents]]\nid = \"allan\"\nname = \"Allan\"\nmodel = \"opus\"\n");
        let overlay = t("[[agents]]\nid = \"allan\"\nname = \"Allan v2\"\n\
                         [[agents]]\nid = \"maple\"\nname = \"Maple\"\n");
        merge_overlay(&mut base, &overlay);
        let agents = base.get("agents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(agents.len(), 2);
        // Per-entity replace: overlay's allan has NO model field left over.
        let allan = &agents[0];
        assert_eq!(allan.get("name").and_then(|v| v.as_str()), Some("Allan v2"));
        assert!(
            allan.get("model").is_none(),
            "replace is wholesale, not field-merge"
        );
        assert_eq!(agents[1].get("id").and_then(|v| v.as_str()), Some("maple"));
    }

    #[test]
    fn agents_remove_drops_base_agents_and_is_consumed() {
        let mut base = t("[[agents]]\nid = \"allan\"\n[[agents]]\nid = \"quill\"\n");
        let overlay = t("agents_remove = [\"quill\"]\n");
        merge_overlay(&mut base, &overlay);
        let agents = base.get("agents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].get("id").and_then(|v| v.as_str()), Some("allan"));
        assert!(
            base.get("agents_remove").is_none(),
            "agents_remove is consumed by the merge"
        );
    }

    #[test]
    fn deep_merge_overlay_wins_and_preserves_siblings() {
        let mut base = t("[llm.routing]\nenabled = false\nthreshold = 5\n");
        let overlay = t("[llm.routing]\nenabled = true\n");
        merge_overlay(&mut base, &overlay);
        let routing = base
            .get("llm")
            .and_then(|v| v.get("routing"))
            .and_then(|v| v.as_table())
            .unwrap();
        assert_eq!(routing.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            routing.get("threshold").and_then(|v| v.as_integer()),
            Some(5),
            "untouched sibling keys survive"
        );
    }

    #[test]
    fn merge_keeps_env_references_unexpanded() {
        // The merge is raw — `${VAR}` survives for promote/export.
        let mut base = t("[llm]\napi_key = \"${OPENROUTER_API_KEY}\"\n");
        let overlay = t("[[agents]]\nid = \"maple\"\n");
        merge_overlay(&mut base, &overlay);
        assert_eq!(
            base.get("llm")
                .and_then(|v| v.get("api_key"))
                .and_then(|v| v.as_str()),
            Some("${OPENROUTER_API_KEY}")
        );
    }

    // ── Provenance / revert / converged ──────────────────────────

    #[test]
    fn overlay_key_paths_cover_agents_and_scalars() {
        let ov = t("agents_remove = [\"old\"]\n\
                    [[agents]]\nid = \"maple\"\n\
                    [llm.routing]\nenabled = true\n");
        let mut paths = overlay_key_paths(&ov);
        paths.sort();
        assert_eq!(
            paths,
            vec!["agents.maple", "agents_remove.old", "llm.routing.enabled"]
        );
    }

    #[test]
    fn converged_detection_flags_redundant_overrides() {
        let base = t("[[agents]]\nid = \"maple\"\nname = \"Maple\"\n\
                      [llm.routing]\nenabled = true\n");
        // Identical agent entry + identical scalar → converged.
        let ov = t("[[agents]]\nid = \"maple\"\nname = \"Maple\"\n\
                    [llm.routing]\nenabled = true\n");
        assert!(key_path_converged(&base, &ov, "agents.maple"));
        assert!(key_path_converged(&base, &ov, "llm.routing.enabled"));

        // Differing entry → still load-bearing.
        let ov2 = t("[[agents]]\nid = \"maple\"\nname = \"Maple v2\"\n\
                     [llm.routing]\nenabled = false\n");
        assert!(!key_path_converged(&base, &ov2, "agents.maple"));
        assert!(!key_path_converged(&base, &ov2, "llm.routing.enabled"));

        // Tombstone converges once the base stops defining the agent.
        let ov3 = t("agents_remove = [\"gone\", \"maple\"]\n");
        assert!(key_path_converged(&base, &ov3, "agents_remove.gone"));
        assert!(!key_path_converged(&base, &ov3, "agents_remove.maple"));
    }

    #[test]
    fn remove_overlay_path_reverts_agents_and_scalars() {
        let mut ov = t("agents_remove = [\"old\"]\n\
                        [[agents]]\nid = \"maple\"\n\
                        [llm.routing]\nenabled = true\n");
        assert!(remove_overlay_path(&mut ov, "agents.maple"));
        assert!(ov.get("agents").is_none());
        assert!(remove_overlay_path(&mut ov, "agents_remove.old"));
        assert!(ov.get("agents_remove").is_none());
        assert!(remove_overlay_path(&mut ov, "llm.routing.enabled"));
        assert!(ov.get("llm").is_none(), "empty parent tables are pruned");
        assert!(!remove_overlay_path(&mut ov, "agents.unknown"));
        assert!(!remove_overlay_path(&mut ov, "llm.routing.enabled"));
    }

    #[test]
    fn overlay_path_expands_data_dir() {
        let base = t("[hub]\ndata_dir = \"/srv/wf-data\"\n");
        assert_eq!(
            overlay_path(&base),
            PathBuf::from("/srv/wf-data/workforce.overlay.toml")
        );
        // Default when [hub] absent.
        assert_eq!(
            overlay_path(&toml::Table::new()),
            PathBuf::from("./data/workforce.overlay.toml")
        );
    }
}
