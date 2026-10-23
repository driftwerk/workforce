//! Per-user configuration resolution with fallback to global.
//!
//! Wraps a [`Vault`] to provide transparent per-user secret namespacing.
//! When an identity is available, lookups check `users/{identity_id}/{key}`
//! first, falling back to `{key}` (global). Writes with a user scope go
//! to the per-user namespace.

use std::collections::HashSet;
use std::sync::Arc;

use workforce_common::secret_lock::RefreshLock;
use workforce_common::session::TurnPrincipal;
use workforce_common::settings::VaultActor;

use crate::vault::{commit, move_value, Vault, VaultOp, VaultWrite};

pub const USER_PREFIX: &str = "users/";

/// Read the canonical identity UUID from the task-local context.
///
/// Returns `Some(uuid)` when the current request originates from a known
/// identity (Slack user, OIDC login, etc.), `None` for system-initiated
/// actions or when identity is not available.
pub fn current_identity_id() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// The identity this turn's spend is charged to: an explicit billing
/// identity when scoped (a job running as a human but billing a `system:*`
/// lane), else the acting identity — never the person background work is
/// billed to, whose plan serves it but whose budget it does not touch
/// ([`TurnPrincipal::billing`]). Cost records and budget admission use
/// this; access decisions (credentials, memory scoping) keep
/// [`current_identity_id`].
pub fn current_billing_identity_id() -> Option<String> {
    current_principal().billing().map(str::to_string)
}

/// The billing identity scoped apart from the acting identity, if any.
pub fn explicit_billing_identity() -> Option<String> {
    workforce_agent::runner::CURRENT_BILLING_IDENTITY
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// Who the work in scope runs as and bills to, to carry where the scope
/// does not reach.
pub fn current_principal() -> TurnPrincipal {
    TurnPrincipal {
        run_as: current_identity_id(),
        bill_to: explicit_billing_identity(),
    }
}

/// Run `fut` as `principal`: its run-as and billing identities replace
/// whatever is in scope, an absent one scoped as no identity.
pub async fn as_principal<F: std::future::Future>(principal: &TurnPrincipal, fut: F) -> F::Output {
    workforce_agent::runner::CURRENT_USER_ID
        .scope(
            principal.run_as.clone().unwrap_or_default(),
            workforce_agent::runner::CURRENT_BILLING_IDENTITY
                .scope(principal.bill_to.clone().unwrap_or_default(), fut),
        )
        .await
}

/// Whose subscription a call made now should draw on: the identity the call
/// runs as when that is a person — never another person it is billed to,
/// which would be the credential sharing `[llm.logins] shared_serves`
/// exists to prevent — else the person its background work is billed to.
pub fn current_payer() -> workforce_common::subscription::Payer {
    workforce_common::subscription::Payer::paying(
        current_identity_id().as_deref(),
        explicit_billing_identity().as_deref(),
    )
}

/// Run background `fut` billed to `person`'s plan, when there is one. The
/// work still runs as the identity it runs as; only whose plan and budget
/// its spend lands on changes.
pub async fn billed_to<F: std::future::Future>(person: Option<uuid::Uuid>, fut: F) -> F::Output {
    match person {
        Some(person) => {
            workforce_agent::runner::CURRENT_BILLING_IDENTITY
                .scope(person.to_string(), fut)
                .await
        }
        None => fut.await,
    }
}

/// Where `uid`'s own copy of `key` lives in a vault.
pub fn per_user_key(uid: &str, key: &str) -> String {
    format!("{USER_PREFIX}{uid}/{key}")
}

/// A vault wrapper that resolves secrets with per-user fallback.
///
/// Resolution chain: `users/{identity_id}/{key}` → `{key}` (global); a
/// strict view ([`UserAwareVault::for_user_strict`]) never falls back.
/// Writes land in the user's namespace. Without an identity it behaves like
/// the underlying vault.
pub struct UserAwareVault {
    vault: Arc<Vault>,
    identity_id: Option<String>,
    shared_fallback: bool,
}

impl UserAwareVault {
    pub fn for_user(vault: Arc<Vault>, identity_id: impl Into<String>) -> Self {
        Self::new(vault, Some(identity_id.into()), true)
    }

    /// A user's own secrets only — for credentials one user must never be
    /// handed from another, such as per-user OAuth tokens.
    pub fn for_user_strict(vault: Arc<Vault>, identity_id: impl Into<String>) -> Self {
        Self::new(vault, Some(identity_id.into()), false)
    }

    pub fn global(vault: Arc<Vault>) -> Self {
        Self::new(vault, None, true)
    }

    pub fn from_current_user(vault: Arc<Vault>) -> Self {
        Self::new(vault, current_identity_id(), true)
    }

    fn new(vault: Arc<Vault>, identity_id: Option<String>, shared_fallback: bool) -> Self {
        Self {
            vault,
            identity_id,
            shared_fallback,
        }
    }

    fn user_path(&self, key: &str) -> Option<String> {
        self.identity_id.as_deref().map(|id| per_user_key(id, key))
    }

    /// Where `key` lives for this user: `users/{uid}/{key}`, or `key` as
    /// given when there is no identity or it already names this user's entry.
    fn own_path(&self, key: &str) -> String {
        self.identity_id
            .as_ref()
            .map(|uid| format!("{USER_PREFIX}{uid}/"))
            .filter(|own| !key.starts_with(own.as_str()))
            .map_or_else(|| key.to_string(), |own| own + key)
    }

    /// `key` in the shared namespace, when this view may fall back to it; a
    /// per-user key has no shared form.
    fn shared_path<'k>(&self, key: &'k str) -> Option<&'k str> {
        (self.shared_fallback && !key.starts_with(USER_PREFIX)).then_some(key)
    }

    pub fn resolve(&self, key: &str) -> Option<String> {
        self.vault
            .get(&self.own_path(key))
            .or_else(|| self.shared_path(key).and_then(|key| self.vault.get(key)))
    }

    pub fn resolve_or_explain(&self, key: &str) -> eyre::Result<String> {
        self.resolve(key).ok_or_else(
            || match (self.identity_id.as_deref(), self.shared_fallback) {
                (Some(uid), true) => eyre::eyre!(
                    "Credential '{key}' not found (checked users/{uid}/{key} and {key})"
                ),
                (Some(uid), false) => {
                    eyre::eyre!("Credential '{key}' not found (checked users/{uid}/{key})")
                }
                (None, _) => eyre::eyre!(
                    "Credential '{key}' not found and no user identity for per-user lookup"
                ),
            },
        )
    }

    pub fn set_for_user(&self, key: &str, value: &str) -> eyre::Result<()> {
        let user_key = self
            .user_path(key)
            .ok_or_else(|| eyre::eyre!("Cannot set per-user secret: no identity available"))?;
        self.vault.set(&user_key, value)
    }

    pub fn list_for_user(&self, prefix: Option<&str>) -> Vec<String> {
        let mut paths = if self.shared_fallback {
            self.vault.list(prefix)
        } else {
            Vec::new()
        };
        if let Some(ref uid) = self.identity_id {
            let user_prefix = format!("{USER_PREFIX}{uid}/");
            let full_prefix = prefix
                .map(|p| format!("{user_prefix}{p}"))
                .unwrap_or_else(|| user_prefix.clone());
            let existing: HashSet<String> = paths.iter().cloned().collect();
            let user_paths: Vec<String> = self
                .vault
                .list(Some(&full_prefix))
                .into_iter()
                .filter_map(|p| p.strip_prefix(&user_prefix).map(String::from))
                .filter(|p| !existing.contains(p))
                .collect();
            paths.extend(user_paths);
        }
        paths
    }
}

/// A `SecretStore` that resolves credentials across the four fixed vault
/// tiers (see [`workforce_common::settings::VaultSource`]):
///   1. `agent_per_user`  — `agent_vault.get("users/{uid}/{path}")`
///   2. `global_per_user` — `global_vault.get("users/{uid}/{path}")`
///   3. `agent_specific`  — `agent_vault.get(path)`
///   4. `global`          — `global_vault.get(path)`
///
/// Reads (`get`, `get_entry`, `inspect`, `get_with_source`) return the
/// most-specific (lowest-numbered) hit. Writes via the legacy `set` /
/// `set_with_actor` default to tier 2 (global_per_user) when an identity is
/// in scope and the global vault is configured, falling back to tier 3 / the
/// agent vault for system contexts; callers wanting explicit tier control
/// use `set_at_tier`.
///
/// Identity is resolved dynamically from the `CURRENT_USER_ID` task-local
/// on every access, so a single store instance is safe to share across
/// requests from different users. In single-vault deployments (no
/// global_vault), tiers 2 and 4 transparently route to the agent vault.
pub struct LayeredUserVault {
    agent_vault: Arc<Vault>,
    global_vault: Option<Arc<Vault>>,
}

impl LayeredUserVault {
    pub fn new(agent_vault: Arc<Vault>, global_vault: Option<Arc<Vault>>) -> Self {
        Self {
            agent_vault,
            global_vault,
        }
    }

    fn user_key(path: &str) -> Option<String> {
        current_identity_id().map(|uid| format!("{USER_PREFIX}{uid}/{path}"))
    }

    /// Resolve the GitHub PAT for a repo owned by `owner`. Delegates to the
    /// single borrow-based resolver [`resolve_github_pat`] so the precedence
    /// (`user-workspace > user-flat > agent-workspace > agent-flat`) lives in
    /// one place.
    pub fn get_github_pat(&self, owner: &str) -> Option<String> {
        resolve_github_pat(
            Some(self.agent_vault.as_ref()),
            self.global_vault.as_deref(),
            owner,
        )
    }

    /// `(tier, vault, physical_key)` triples in resolution order for `path`,
    /// for the active identity. Per-user tiers only appear when an identity
    /// is in scope. Push order is already the resolution order so no sort.
    fn tiers_in_order_for(
        &self,
        path: &str,
    ) -> Vec<(workforce_common::settings::VaultSource, &Vault, String)> {
        layered_tiers(
            Some(self.agent_vault.as_ref()),
            self.global_vault.as_deref(),
            path,
        )
    }

    /// Map a (path, tier) write target to `(vault, physical_key)`. Per-user
    /// tiers (T1/T2) require an active identity and reject paths that
    /// already include a `users/<uid>/` segment to avoid double-prefixing;
    /// bare tiers (T3/T4) accept any path verbatim. T2/T4 fall back to the
    /// agent vault when no global vault is configured, keeping single-vault
    /// CLI deployments working without caller awareness.
    fn resolve_write(
        &self,
        path: &str,
        tier: workforce_common::settings::VaultSource,
    ) -> eyre::Result<(&Arc<Vault>, String)> {
        use workforce_common::settings::VaultSource;
        let per_user_key = || {
            if path.starts_with(USER_PREFIX) {
                return Err(eyre::eyre!(
                    "vault path `{path}` already includes a `users/<uid>/` segment; \
                     pass the logical path without the per-user prefix for tier `{}`",
                    tier.label()
                ));
            }
            Self::user_key(path).ok_or_else(|| {
                eyre::eyre!("tier `{}` requires an active user identity", tier.label())
            })
        };
        let global = self.global_vault.as_ref().unwrap_or(&self.agent_vault);
        match tier {
            VaultSource::AgentPerUser => Ok((&self.agent_vault, per_user_key()?)),
            VaultSource::GlobalPerUser => Ok((global, per_user_key()?)),
            VaultSource::AgentSpecific => Ok((&self.agent_vault, path.to_string())),
            VaultSource::Global => Ok((global, path.to_string())),
            VaultSource::Unknown => Err(eyre::eyre!("cannot address tier `unknown`")),
        }
    }

    /// The vault and key a write of `path` at `tier` lands on, for the
    /// identity in scope.
    pub fn write_target(
        &self,
        path: &str,
        tier: workforce_common::settings::VaultSource,
    ) -> eyre::Result<(Arc<Vault>, String)> {
        self.resolve_write(path, tier)
            .map(|(vault, key)| (Arc::clone(vault), key))
    }

    /// The vault and key `path` resolves to for the identity in scope: the
    /// most specific tier that holds a value, which is what a delete removes.
    pub fn locate(&self, path: &str) -> Option<(Arc<Vault>, String)> {
        self.tiers_in_order_for(path)
            .into_iter()
            .find(|(_, vault, key)| vault.get(key).is_some())
            .map(|(_, vault, key)| {
                let held = std::iter::once(&self.agent_vault)
                    .chain(self.global_vault.as_ref())
                    .find(|held| std::ptr::eq(held.as_ref(), vault))
                    .unwrap_or(&self.agent_vault);
                (Arc::clone(held), key)
            })
    }

    /// Default write tier for the legacy `set` / `set_with_actor` entry
    /// points: T2 when an identity is in scope (per-user creds default to
    /// global so they're agent-portable), T3 otherwise (system jobs land in
    /// the per-agent shared namespace they used pre-overhaul).
    fn default_write_tier(&self, path: &str) -> workforce_common::settings::VaultSource {
        use workforce_common::settings::VaultSource;
        if path.starts_with(USER_PREFIX) {
            return VaultSource::AgentSpecific;
        }
        match Self::user_key(path) {
            Some(_) => VaultSource::GlobalPerUser,
            None => VaultSource::AgentSpecific,
        }
    }

    /// List entries at a user-scoped tier (T1/T2). `prefix` is the bare
    /// logical prefix; the per-user `users/{uid}/` segment is added before
    /// the list and stripped from the results.
    fn list_user_scoped(
        vault: &Vault,
        user_prefix: Option<&str>,
        prefix: Option<&str>,
    ) -> Vec<(String, workforce_common::settings::VaultEntry)> {
        let Some(user_prefix) = user_prefix else {
            return Vec::new();
        };
        let full = match prefix {
            Some(p) => format!("{user_prefix}{p}"),
            None => user_prefix.to_string(),
        };
        vault
            .list(Some(&full))
            .into_iter()
            .filter_map(|raw| {
                let stripped = raw.strip_prefix(user_prefix)?.to_string();
                vault.get_entry(&raw).map(|e| (stripped, e))
            })
            .collect()
    }

    /// List entries at a bare tier (T3/T4). `prefix` is the bare logical
    /// prefix. Skips entries that start with `users/` since those belong to
    /// the user-scoped tiers above.
    fn list_bare(
        vault: &Vault,
        prefix: Option<&str>,
    ) -> Vec<(String, workforce_common::settings::VaultEntry)> {
        vault
            .list(prefix)
            .into_iter()
            .filter(|p| !p.starts_with(USER_PREFIX))
            .filter_map(|p| vault.get_entry(&p).map(|e| (p, e)))
            .collect()
    }
}

impl workforce_common::settings::SecretStore for LayeredUserVault {
    fn get(&self, path: &str) -> Option<String> {
        self.tiers_in_order_for(path)
            .into_iter()
            .find_map(|(_, vault, key)| vault.get(&key))
    }

    fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
        self.set_with_actor(path, value, default_set_actor())
    }

    fn set_with_actor(
        &self,
        path: &str,
        value: &str,
        actor: workforce_common::settings::VaultActor<'_>,
    ) -> eyre::Result<()> {
        let tier = self.default_write_tier(path);
        self.set_at_tier(path, value, tier, actor)
    }

    fn get_entry(&self, path: &str) -> Option<workforce_common::settings::VaultEntry> {
        self.tiers_in_order_for(path)
            .into_iter()
            .find_map(|(_, vault, key)| vault.get_entry(&key))
    }

    fn get_with_source(
        &self,
        path: &str,
    ) -> Option<(String, workforce_common::settings::VaultSource)> {
        self.tiers_in_order_for(path)
            .into_iter()
            .find_map(|(tier, vault, key)| vault.get(&key).map(|v| (v, tier)))
    }

    fn inspect(&self, path: &str) -> Vec<(workforce_common::settings::VaultSource, String)> {
        self.tiers_in_order_for(path)
            .into_iter()
            .filter_map(|(tier, vault, key)| vault.get(&key).map(|v| (tier, v)))
            .collect()
    }

    fn delete(&self, path: &str) -> eyre::Result<bool> {
        for (_tier, vault, key) in self.tiers_in_order_for(path) {
            if vault.get(&key).is_some() {
                return vault.delete(&key);
            }
        }
        Ok(false)
    }

    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut paths: Vec<String> = Vec::new();
        for tier in workforce_common::settings::VaultSource::all() {
            for (path, _) in self.list_at_tier(prefix, tier) {
                if seen.insert(path.clone()) {
                    paths.push(path);
                }
            }
        }
        paths
    }

    fn get_user_override(&self, path: &str) -> Option<String> {
        user_override_layered(
            Some(self.agent_vault.as_ref()),
            self.global_vault.as_deref(),
            path,
        )
    }

    fn set_at_tier(
        &self,
        path: &str,
        value: &str,
        tier: workforce_common::settings::VaultSource,
        actor: workforce_common::settings::VaultActor<'_>,
    ) -> eyre::Result<()> {
        let (vault, key) = self.resolve_write(path, tier)?;
        vault.set_with_actor(&key, value, actor)
    }

    fn delete_at_tier(
        &self,
        path: &str,
        tier: workforce_common::settings::VaultSource,
    ) -> eyre::Result<bool> {
        let (vault, key) = self.resolve_write(path, tier)?;
        vault.delete(&key)
    }

    fn list_at_tier(
        &self,
        prefix: Option<&str>,
        tier: workforce_common::settings::VaultSource,
    ) -> Vec<(String, workforce_common::settings::VaultEntry)> {
        use workforce_common::settings::VaultSource;
        let user_prefix = current_identity_id().map(|uid| format!("{USER_PREFIX}{uid}/"));
        match tier {
            VaultSource::AgentPerUser => {
                Self::list_user_scoped(self.agent_vault.as_ref(), user_prefix.as_deref(), prefix)
            }
            VaultSource::GlobalPerUser => self
                .global_vault
                .as_deref()
                .map(|g| Self::list_user_scoped(g, user_prefix.as_deref(), prefix))
                .unwrap_or_default(),
            VaultSource::AgentSpecific => Self::list_bare(self.agent_vault.as_ref(), prefix),
            VaultSource::Global => self
                .global_vault
                .as_deref()
                .map(|g| Self::list_bare(g, prefix))
                .unwrap_or_default(),
            VaultSource::Unknown => Vec::new(),
        }
    }

    fn set_many(&self, entries: &[(&str, &str)], actor: VaultActor<'_>) -> eyre::Result<()> {
        let targets = entries
            .iter()
            .map(|&(path, value)| {
                self.resolve_write(path, self.default_write_tier(path))
                    .map(|(vault, key)| (vault, key, value))
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let writes: Vec<VaultWrite<'_>> = targets
            .iter()
            .map(|(vault, key, value)| VaultWrite {
                vault,
                op: VaultOp::Set { path: key, value },
            })
            .collect();
        commit(&writes, actor).map(drop)
    }

    fn sync(&self) -> eyre::Result<()> {
        self.agent_vault.sync()?;
        self.global_vault
            .as_deref()
            .map_or(Ok(()), |vault| vault.sync())
    }

    fn write_location(&self, path: &str) -> String {
        self.resolve_write(path, self.default_write_tier(path))
            .map_or_else(
                |_| path.to_string(),
                |(vault, key)| vault.write_location(&key),
            )
    }

    fn refresh_lock(&self) -> Arc<dyn RefreshLock> {
        self.agent_vault.refresh_lock()
    }

    fn move_at_tier(
        &self,
        from: (&str, workforce_common::settings::VaultSource),
        to: (&str, workforce_common::settings::VaultSource),
        value: &str,
        actor: VaultActor<'_>,
    ) -> eyre::Result<()> {
        let (source_vault, source_key) = self.resolve_write(from.0, from.1)?;
        let (dest_vault, dest_key) = self.resolve_write(to.0, to.1)?;
        move_value(
            (source_vault, &source_key),
            (dest_vault, &dest_key),
            value,
            actor,
        )
    }
}

/// Default actor recorded for legacy `SecretStore::set` calls — `User` when
/// an identity is in scope, `System("unknown")` otherwise. Mirrors the
/// existing convention in `Vault::set`.
fn default_set_actor() -> workforce_common::settings::VaultActor<'static> {
    use workforce_common::settings::VaultActor;
    // `LayeredUserVault::set_with_actor` reads the active identity at the
    // resolver call site, so passing `System("unknown")` here is fine — the
    // physical write tier is decided from `current_identity_id()` independently.
    VaultActor::System("layered_vault_set")
}

/// Single source of truth for the four-tier resolution ORDER, over borrowed
/// vaults. [`LayeredUserVault`] and every borrow-based resolver below delegate
/// here so the precedence (`agent-per-user > global-per-user > agent > global`)
/// is defined in exactly one place. Per-user tiers appear only when an
/// identity is in scope; an absent vault is skipped — so a `(None, global)`
/// pair resolves the two global tiers, and a `(agent, None)` pair the two
/// agent tiers.
pub fn layered_tiers<'a>(
    agent: Option<&'a Vault>,
    global: Option<&'a Vault>,
    path: &str,
) -> Vec<(workforce_common::settings::VaultSource, &'a Vault, String)> {
    use workforce_common::settings::VaultSource;
    let mut out = Vec::with_capacity(4);
    let user_key = current_identity_id().map(|uid| format!("{USER_PREFIX}{uid}/{path}"));
    if let Some(ref k) = user_key {
        if let Some(a) = agent {
            out.push((VaultSource::AgentPerUser, a, k.clone()));
        }
        if let Some(g) = global {
            out.push((VaultSource::GlobalPerUser, g, k.clone()));
        }
    }
    if let Some(a) = agent {
        out.push((VaultSource::AgentSpecific, a, path.to_string()));
    }
    if let Some(g) = global {
        out.push((VaultSource::Global, g, path.to_string()));
    }
    out
}

/// Resolve `path` across the four tiers (most-specific first) over borrowed
/// vaults — the borrow-based twin of [`LayeredUserVault::get`], for call sites
/// that hold `Option<&Vault>` pairs rather than a constructed store.
pub fn get_layered(agent: Option<&Vault>, global: Option<&Vault>, path: &str) -> Option<String> {
    layered_tiers(agent, global, path)
        .into_iter()
        .find_map(|(_, vault, key)| vault.get(&key))
}

/// Resolve only the per-user overlay tiers for `path` (global-per-user then
/// agent-per-user), or `None` without an identity in scope. The borrow-based
/// twin of [`LayeredUserVault::get_user_override`]; the per-user precedence is
/// deliberately the same so a user's value wins regardless of which vault
/// holds it.
pub fn user_override_layered(
    agent: Option<&Vault>,
    global: Option<&Vault>,
    path: &str,
) -> Option<String> {
    let user_key = current_identity_id().map(|uid| format!("{USER_PREFIX}{uid}/{path}"))?;
    global
        .and_then(|g| g.get(&user_key))
        .or_else(|| agent.and_then(|a| a.get(&user_key)))
}

/// Resolve a flat (non-workspace) credential `path` with the per-user overlay
/// winning over the bare tiers — the single-form twin of [`resolve_github_pat`].
/// Preserves the convention that a user's value (preferring the My-Settings
/// global vault) wins over an agent-shared one, while still reaching the bare
/// agent → global tiers.
pub fn resolve_flat(agent: Option<&Vault>, global: Option<&Vault>, path: &str) -> Option<String> {
    resolve_flat_from(agent, global, path).map(|hit| hit.value)
}

/// What [`resolve_flat_from`] found, and where.
pub struct FlatHit<'a, V> {
    pub source: workforce_common::settings::VaultSource,
    pub vault: &'a V,
    pub key: String,
    pub value: String,
}

/// [`resolve_flat`], naming the tier, vault and key the value came from.
pub fn resolve_flat_from<'a, V: std::borrow::Borrow<Vault>>(
    agent: Option<&'a V>,
    global: Option<&'a V>,
    path: &str,
) -> Option<FlatHit<'a, V>> {
    use workforce_common::settings::VaultSource;
    let personal = current_identity_id().into_iter().flat_map(|uid| {
        let key = per_user_key(&uid, path);
        [
            (VaultSource::GlobalPerUser, global, key.clone()),
            (VaultSource::AgentPerUser, agent, key),
        ]
    });
    personal
        .chain([
            (VaultSource::AgentSpecific, agent, path.to_string()),
            (VaultSource::Global, global, path.to_string()),
        ])
        .find_map(|(source, vault, key)| {
            let vault = vault?;
            let value = vault.borrow().get(&key)?;
            Some(FlatHit {
                source,
                vault,
                key,
                value,
            })
        })
}

/// The single resolver for every GitHub PAT read — clone/fetch/push, the
/// Claude Code subprocess, knowledge indexing and workflow runtime.
///
/// Precedence: `user-workspace > user-flat > agent-workspace > agent-flat`,
/// each form resolved across all four tiers. A per-user override at either
/// form wins over any agent-shared value, closing the cross-form shadow trap.
/// Without an identity in scope (system jobs) the per-user tiers are simply
/// absent, leaving `agent-workspace > agent-flat > global-workspace >
/// global-flat`.
pub fn resolve_github_pat(
    agent_vault: Option<&Vault>,
    global_vault: Option<&Vault>,
    owner: &str,
) -> Option<String> {
    let workspace = format!("skills/github/workspaces/{owner}/token");
    user_override_layered(agent_vault, global_vault, &workspace)
        .or_else(|| user_override_layered(agent_vault, global_vault, GITHUB_FLAT_TOKEN))
        .or_else(|| get_layered(agent_vault, global_vault, &workspace))
        .or_else(|| get_layered(agent_vault, global_vault, GITHUB_FLAT_TOKEN))
}

/// Vault path of the GitHub PAT that isn't scoped to one org.
const GITHUB_FLAT_TOKEN: &str = "skills/github/token";

/// The GitHub PAT for a session with no repo to key an owner off — an open
/// bench, where Claude Code clones whatever it is asked for. Same precedence
/// as [`resolve_github_pat`]'s flat half; `None` means the bench has no
/// GitHub access, which callers surface rather than letting a clone fail.
pub fn resolve_bench_github_pat(
    agent_vault: Option<&Vault>,
    global_vault: Option<&Vault>,
) -> Option<String> {
    resolve_flat(agent_vault, global_vault, GITHUB_FLAT_TOKEN)
}

impl workforce_common::settings::VaultResolver for UserAwareVault {
    fn resolve(&self, path: &str) -> Option<String> {
        UserAwareVault::resolve(self, path)
    }
}

impl workforce_common::settings::SecretStore for UserAwareVault {
    fn get(&self, path: &str) -> Option<String> {
        UserAwareVault::resolve(self, path)
    }

    fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
        self.vault.set(&self.own_path(path), value)
    }

    fn set_with_actor(&self, path: &str, value: &str, actor: VaultActor<'_>) -> eyre::Result<()> {
        self.vault
            .set_with_actor(&self.own_path(path), value, actor)
    }

    fn get_entry(&self, path: &str) -> Option<workforce_common::settings::VaultEntry> {
        self.vault.get_entry(&self.own_path(path)).or_else(|| {
            self.shared_path(path)
                .and_then(|path| self.vault.get_entry(path))
        })
    }

    fn delete(&self, path: &str) -> eyre::Result<bool> {
        self.vault.delete(&self.own_path(path))
    }

    fn list(&self, prefix: Option<&str>) -> Vec<String> {
        self.list_for_user(prefix)
    }

    fn set_many(&self, entries: &[(&str, &str)], actor: VaultActor<'_>) -> eyre::Result<()> {
        let owned: Vec<(String, &str)> = entries
            .iter()
            .map(|&(path, value)| (self.own_path(path), value))
            .collect();
        let entries: Vec<(&str, &str)> = owned
            .iter()
            .map(|(path, value)| (path.as_str(), *value))
            .collect();
        self.vault.set_many(&entries, actor)
    }

    fn sync(&self) -> eyre::Result<()> {
        self.vault.sync()
    }

    fn write_location(&self, path: &str) -> String {
        self.vault.write_location(&self.own_path(path))
    }

    fn refresh_lock(&self) -> Arc<dyn RefreshLock> {
        self.vault.refresh_lock()
    }
}

#[cfg(test)]
mod layered_tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::settings::SecretStore;

    fn new_vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        // Keep the tempdir alive for the test by leaking — OS cleans up
        // temp files on process exit. Tests are short-lived.
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    /// Tripwire (not a guarantee — a bound `let k = "skills/github/…"` dodges
    /// it): GitHub tokens must resolve through this module's layered resolvers,
    /// never as a raw `Vault::get("skills/github/…")` chain — the flat-only,
    /// per-user-blind pattern that was the coding.session auth bug. Resolver
    /// calls on a `store`/`layered` `SecretStore` are fine and not matched.
    #[test]
    fn no_raw_vault_github_token_reads_in_hub() {
        const RAW: &[&str] = &[
            "vault.get(\"skills/github",
            "v.get(\"skills/github",
            "global_vault.get(\"skills/github",
            "agent_vault.get(\"skills/github",
        ];
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, hits);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // The resolvers themselves legitimately name these paths.
                if path.file_name().and_then(|n| n.to_str()) == Some("user_config.rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).unwrap();
                for (i, line) in src.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    if RAW.iter().any(|p| line.contains(p)) {
                        hits.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut hits = Vec::new();
        for krate in ["hub-core", "hub"] {
            let src_root = crates.join(krate).join("src");
            assert!(
                src_root.is_dir(),
                "{} is not a directory",
                src_root.display()
            );
            walk(&src_root, &mut hits);
        }
        assert!(
            hits.is_empty(),
            "raw Vault GitHub-token reads must route through resolve_github_pat/get_layered: {hits:#?}"
        );
    }

    #[tokio::test]
    async fn global_user_overlay_wins_over_agent_shared() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "tom-shared").unwrap();
        global
            .set("users/kings/skills/jira/api_token", "kings-personal")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get("skills/jira/api_token").as_deref(),
                    Some("kings-personal")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn falls_back_to_agent_user_overlay_when_global_missing() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/kings/skills/jira/api_token", "kings-legacy")
            .unwrap();
        agent.set("skills/jira/api_token", "tom-shared").unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get("skills/jira/api_token").as_deref(),
                    Some("kings-legacy")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn falls_back_to_agent_shared_when_no_user_overlay() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "tom-shared").unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get("skills/jira/api_token").as_deref(),
                    Some("tom-shared")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn no_identity_reads_agent_shared_only() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "tom-shared").unwrap();
        global
            .set("users/kings/skills/jira/api_token", "kings-personal")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        assert_eq!(
            store.get("skills/jira/api_token").as_deref(),
            Some("tom-shared")
        );
    }

    /// Regression: org-wide shared secrets (OAuth client creds, etc.) live at
    /// the bare path in the global vault. The previous three-step fallback
    /// never reached there, so admins storing `skills/google/oauth_client/*`
    /// in global saw skills fail with "creds not found".
    #[tokio::test]
    async fn falls_back_to_global_shared_when_nothing_scoped_exists() {
        let agent = new_vault();
        let global = new_vault();
        global
            .set("skills/google/oauth_client/client_id", "org-client-id")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async {
                assert_eq!(
                    store.get("skills/google/oauth_client/client_id").as_deref(),
                    Some("org-client-id")
                );
            })
            .await;

        assert_eq!(
            store.get("skills/google/oauth_client/client_id").as_deref(),
            Some("org-client-id")
        );
    }

    /// Precedence: an agent-shared bare path (step 3) beats the new
    /// global-shared bare path (step 4), so an agent can still override an
    /// org-wide default.
    #[tokio::test]
    async fn agent_shared_bare_beats_global_shared_bare() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/google/oauth_client/client_id", "agent-override")
            .unwrap();
        global
            .set("skills/google/oauth_client/client_id", "org-default")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        assert_eq!(
            store.get("skills/google/oauth_client/client_id").as_deref(),
            Some("agent-override")
        );
    }

    #[tokio::test]
    async fn set_with_identity_writes_to_global_vault() {
        let agent = new_vault();
        let global = new_vault();

        let store = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("kings".to_string(), async {
                store.set("skills/jira/api_token", "new-token").unwrap();
            })
            .await;

        assert_eq!(
            global.get("users/kings/skills/jira/api_token").as_deref(),
            Some("new-token")
        );
        assert!(agent.get("skills/jira/api_token").is_none());
        assert!(agent.get("users/kings/skills/jira/api_token").is_none());
    }

    #[tokio::test]
    async fn set_without_identity_writes_to_agent_shared() {
        let agent = new_vault();
        let global = new_vault();

        let store = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));
        store.set("skills/jira/api_token", "shared-token").unwrap();

        assert_eq!(
            agent.get("skills/jira/api_token").as_deref(),
            Some("shared-token")
        );
        assert!(global.list(None).is_empty());
    }

    #[tokio::test]
    async fn delete_targets_most_specific_tier_first() {
        // `delete` follows the same most-specific-wins rule as `get`: an
        // agent_per_user (T1) entry shadows global_per_user (T2), so a
        // bare `delete("...")` removes T1 and leaves T2 untouched.
        // Callers wanting to drop a specific tier use `delete_at_tier`.
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/kings/skills/jira/api_token", "agent-per-user")
            .unwrap();
        global
            .set("users/kings/skills/jira/api_token", "global-per-user")
            .unwrap();

        let store = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("kings".to_string(), async {
                assert!(store.delete("skills/jira/api_token").unwrap());
            })
            .await;

        assert!(agent.get("users/kings/skills/jira/api_token").is_none());
        assert_eq!(
            global.get("users/kings/skills/jira/api_token").as_deref(),
            Some("global-per-user"),
            "delete should not cascade past the most-specific tier"
        );
    }

    #[tokio::test]
    async fn list_includes_global_bare_paths_without_user_identity() {
        // Mirrors `get`'s tier-4 fallback: an admin-managed workspace placed
        // at a bare path in the global vault must be discoverable by a skill
        // loader running at hub startup with no `CURRENT_USER_ID` set.
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/jira/workspaces/agent-only/api_token", "a")
            .unwrap();
        global
            .set("skills/jira/workspaces/global-shared/api_token", "g")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));
        let mut paths = store.list(Some("skills/jira/workspaces/"));
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "skills/jira/workspaces/agent-only/api_token".to_string(),
                "skills/jira/workspaces/global-shared/api_token".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn list_merges_all_three_layers_without_duplicates() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "shared").unwrap();
        agent
            .set("users/kings/skills/jira/email", "agent-email")
            .unwrap();
        global
            .set("users/kings/skills/jira/api_token", "personal")
            .unwrap();
        global
            .set(
                "users/kings/skills/jira/site",
                "https://example.atlassian.net",
            )
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                let mut paths = store.list(Some("skills/jira/"));
                paths.sort();
                assert_eq!(
                    paths,
                    vec![
                        "skills/jira/api_token".to_string(),
                        "skills/jira/email".to_string(),
                        "skills/jira/site".to_string(),
                    ]
                );
            })
            .await;
    }

    #[tokio::test]
    async fn empty_identity_string_treated_as_no_identity() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "shared").unwrap();
        global
            .set("users//skills/jira/api_token", "would-be-overlay")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope(String::new(), async move {
                assert_eq!(
                    store.get("skills/jira/api_token").as_deref(),
                    Some("shared"),
                    "empty identity must not match `users//...` overlay"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_user_override_returns_only_user_scoped_entry() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/jira/workspaces/default/email", "tom@acme.com")
            .unwrap();
        global
            .set("users/kings/skills/jira/email", "kings@acme.com")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get_user_override("skills/jira/email").as_deref(),
                    Some("kings@acme.com")
                );
                assert_eq!(
                    store
                        .get_user_override("skills/jira/workspaces/default/email")
                        .as_deref(),
                    None,
                    "workspace path has no user override, only shared"
                );
                assert_eq!(
                    store.get("skills/jira/workspaces/default/email").as_deref(),
                    Some("tom@acme.com"),
                    "regular get still resolves shared workspace creds"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_user_override_returns_none_without_identity() {
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/email", "shared@acme.com").unwrap();
        global
            .set("users/kings/skills/jira/email", "kings@acme.com")
            .unwrap();

        let store = LayeredUserVault::new(agent, Some(global));

        assert_eq!(store.get_user_override("skills/jira/email"), None);
    }

    #[tokio::test]
    async fn set_with_actor_records_user_actor_with_identity() {
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(agent, Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                store
                    .set_with_actor(
                        "skills/jira/api_token",
                        "tok",
                        workforce_common::settings::VaultActor::User("kings"),
                    )
                    .unwrap();
            })
            .await;

        let entry = global
            .get_entry("users/kings/skills/jira/api_token")
            .unwrap();
        assert_eq!(entry.value, "tok");
        assert_eq!(entry.created_by, "user:kings");
    }

    #[tokio::test]
    async fn set_with_actor_routes_system_to_agent_vault_without_identity() {
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(Arc::clone(&agent), Some(global));

        store
            .set_with_actor(
                "engagement/overrides",
                "{}",
                workforce_common::settings::VaultActor::System("test_job"),
            )
            .unwrap();

        let entry = agent.get_entry("engagement/overrides").unwrap();
        assert_eq!(entry.created_by, "system:test_job");
    }

    #[tokio::test]
    async fn get_entry_walks_layered_chain() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set_with_actor(
                "skills/google/oauth_client/client_id",
                "global-id",
                workforce_common::settings::VaultActor::Cli,
            )
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        let entry = store
            .get_entry("skills/google/oauth_client/client_id")
            .unwrap();
        assert_eq!(entry.value, "global-id");
        assert_eq!(entry.created_by, "cli");
    }

    #[tokio::test]
    async fn get_with_source_returns_global_per_user_first() {
        use workforce_common::settings::VaultSource;
        let agent = new_vault();
        let global = new_vault();
        global
            .set("users/kings/skills/jira/api_token", "kings-personal")
            .unwrap();
        agent.set("skills/jira/api_token", "tom-shared").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                let (value, source) = store.get_with_source("skills/jira/api_token").unwrap();
                assert_eq!(value, "kings-personal");
                assert_eq!(source, VaultSource::GlobalPerUser);
            })
            .await;
    }

    #[tokio::test]
    async fn get_with_source_agent_per_user_wins_over_global_per_user() {
        use workforce_common::settings::VaultSource;
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/kings/skills/jira/api_token", "kings-agent")
            .unwrap();
        global
            .set("users/kings/skills/jira/api_token", "kings-global")
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                let (value, source) = store.get_with_source("skills/jira/api_token").unwrap();
                assert_eq!(value, "kings-agent");
                assert_eq!(source, VaultSource::AgentPerUser);
            })
            .await;
    }

    #[tokio::test]
    async fn get_with_source_falls_back_to_agent_specific() {
        use workforce_common::settings::VaultSource;
        let agent = new_vault();
        let global = new_vault();
        agent.set("skills/jira/api_token", "shared-tok").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                let (value, source) = store.get_with_source("skills/jira/api_token").unwrap();
                assert_eq!(value, "shared-tok");
                assert_eq!(source, VaultSource::AgentSpecific);
            })
            .await;
    }

    #[tokio::test]
    async fn get_with_source_falls_back_to_global() {
        use workforce_common::settings::VaultSource;
        let agent = new_vault();
        let global = new_vault();
        global
            .set("skills/google/oauth_client/client_id", "org-wide")
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        let (value, source) = store
            .get_with_source("skills/google/oauth_client/client_id")
            .unwrap();
        assert_eq!(value, "org-wide");
        assert_eq!(source, VaultSource::Global);
    }

    #[tokio::test]
    async fn get_with_source_returns_none_for_missing_path() {
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(agent, Some(global));
        assert!(store.get_with_source("missing").is_none());
    }

    #[tokio::test]
    async fn inspect_returns_all_tiers_with_shadows() {
        use workforce_common::settings::VaultSource;
        let agent = new_vault();
        let global = new_vault();
        global
            .set(
                "users/kings/skills/google/oauth_client/client_id",
                "kings-global-per-user",
            )
            .unwrap();
        agent
            .set("skills/google/oauth_client/client_id", "tom-agent-specific")
            .unwrap();
        global
            .set("skills/google/oauth_client/client_id", "kings-global")
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        let hits = CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                store.inspect("skills/google/oauth_client/client_id")
            })
            .await;
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].0, VaultSource::GlobalPerUser);
        assert_eq!(hits[0].1, "kings-global-per-user");
        assert_eq!(hits[1].0, VaultSource::AgentSpecific);
        assert_eq!(hits[1].1, "tom-agent-specific");
        assert_eq!(hits[2].0, VaultSource::Global);
        assert_eq!(hits[2].1, "kings-global");
    }

    #[tokio::test]
    async fn get_at_tier_pins_to_one_tier_without_fallthrough() {
        use workforce_common::settings::{SecretStore, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        // Value only at the Global tier.
        global.set("api_key", "global-only").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                // Pinned read at Global resolves; the agent tier does NOT fall
                // through to it (single-tier pinning — no scope widening).
                assert_eq!(
                    store.get_at_tier("api_key", VaultSource::Global).as_deref(),
                    Some("global-only")
                );
                assert!(store
                    .get_at_tier("api_key", VaultSource::AgentSpecific)
                    .is_none());
                // Per-user tiers resolve nothing when no value is set there.
                assert!(store
                    .get_at_tier("api_key", VaultSource::AgentPerUser)
                    .is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn secret_resolver_end_to_end_against_real_layered_vault() {
        // Proves the full chain — SecretPolicy + resolve_secret_markers +
        // get_at_tier — works against a real LayeredUserVault, not just the
        // resolver's unit-test mock.
        use workforce_common::secret_policy::{
            resolve_secret_markers, DeclaredSecret, SecretPolicy, SECRET_POLICY,
        };
        use workforce_common::settings::SecretStore;

        let agent = new_vault();
        let global = new_vault();
        // Owner 'alice' has a per-user secret (stored at its physical path, as
        // the AgentPerUser tier writes it); an operator places a curated shared
        // secret on the agent tier.
        agent.set("users/alice/portal_pw", "p3rs0nal").unwrap();
        agent.set("workflow_shared/api_key", "sh4red").unwrap();
        let store: Arc<dyn SecretStore> = Arc::new(LayeredUserVault::new(agent, Some(global)));

        let declared = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(s, k)| ((s.to_string(), k.to_string()), DeclaredSecret::default()))
                .collect()
        };

        // Owner run: both the per-user and curated-shared secrets resolve.
        let owner_policy = SecretPolicy {
            per_user_identity: Some("alice".to_string()),
            shared_secret_prefix: Some("workflow_shared/".to_string()),
            declared: declared(&[
                ("agent_user", "portal_pw"),
                ("agent", "workflow_shared/api_key"),
            ]),
        };
        let store2 = Arc::clone(&store);
        SECRET_POLICY
            .scope(owner_policy, async move {
                assert_eq!(
                    resolve_secret_markers("[[vault:agent_user:portal_pw]]", &store2, None)
                        .unwrap(),
                    "p3rs0nal"
                );
                assert_eq!(
                    resolve_secret_markers(
                        "[[vault:agent:workflow_shared/api_key]]",
                        &store2,
                        None
                    )
                    .unwrap(),
                    "sh4red"
                );
            })
            .await;

        // Non-owner run: the per-user secret is refused; the shared one still resolves.
        let other_policy = SecretPolicy {
            per_user_identity: None,
            shared_secret_prefix: Some("workflow_shared/".to_string()),
            declared: declared(&[
                ("agent_user", "portal_pw"),
                ("agent", "workflow_shared/api_key"),
            ]),
        };
        SECRET_POLICY
            .scope(other_policy, async move {
                assert!(
                    resolve_secret_markers("[[vault:agent_user:portal_pw]]", &store, None).is_err()
                );
                assert_eq!(
                    resolve_secret_markers("[[vault:agent:workflow_shared/api_key]]", &store, None)
                        .unwrap(),
                    "sh4red"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn per_user_secret_readable_via_explicit_path_at_bare_tier() {
        use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(Arc::clone(&agent), Some(global));
        // Owner 'alice' stores a per-user secret (as /api/me/vault would).
        CURRENT_USER_ID
            .scope("alice".to_string(), async {
                store
                    .set_at_tier(
                        "portal_pw",
                        "s3cret",
                        VaultSource::AgentPerUser,
                        VaultActor::User("alice"),
                    )
                    .unwrap();
            })
            .await;
        // A workflow turn runs under a synthetic id ("workflow-engine"), yet the
        // secret resolver reads alice's secret by its explicit physical path at
        // the bare tier — independent of whatever identity the turn carries.
        CURRENT_USER_ID
            .scope("workflow-engine".to_string(), async move {
                assert_eq!(
                    store
                        .get_at_tier("users/alice/portal_pw", VaultSource::AgentSpecific)
                        .as_deref(),
                    Some("s3cret")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn set_at_tier_writes_to_each_quadrant_independently() {
        use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("kings".to_string(), async {
                store
                    .set_at_tier(
                        "skills/jira/api_token",
                        "t1",
                        VaultSource::AgentPerUser,
                        VaultActor::User("kings"),
                    )
                    .unwrap();
                store
                    .set_at_tier(
                        "skills/jira/api_token",
                        "t2",
                        VaultSource::GlobalPerUser,
                        VaultActor::User("kings"),
                    )
                    .unwrap();
                store
                    .set_at_tier(
                        "skills/jira/api_token",
                        "t3",
                        VaultSource::AgentSpecific,
                        VaultActor::User("kings"),
                    )
                    .unwrap();
                store
                    .set_at_tier(
                        "skills/jira/api_token",
                        "t4",
                        VaultSource::Global,
                        VaultActor::User("kings"),
                    )
                    .unwrap();
            })
            .await;

        assert_eq!(
            agent.get("users/kings/skills/jira/api_token").as_deref(),
            Some("t1")
        );
        assert_eq!(
            global.get("users/kings/skills/jira/api_token").as_deref(),
            Some("t2")
        );
        assert_eq!(agent.get("skills/jira/api_token").as_deref(), Some("t3"));
        assert_eq!(global.get("skills/jira/api_token").as_deref(), Some("t4"));
    }

    #[tokio::test]
    async fn set_at_tier_rejects_per_user_tier_without_identity() {
        use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(agent, Some(global));

        for tier in [VaultSource::AgentPerUser, VaultSource::GlobalPerUser] {
            let err = store
                .set_at_tier("skills/jira/api_token", "v", tier, VaultActor::Cli)
                .unwrap_err();
            assert!(
                err.to_string().contains("active user identity"),
                "tier {:?} did not reject without identity: {err}",
                tier
            );
        }
    }

    #[tokio::test]
    async fn set_at_tier_rejects_user_prefixed_path_for_per_user_tiers() {
        // Per-user tiers (T1/T2) synthesize a `users/{uid}/...` key from the
        // active identity, so accepting a path that already starts with
        // `users/` would write to `users/{uid}/users/<other>/...`. Bare tiers
        // (T3/T4) accept any path verbatim — including legitimate
        // `users/...`-prefixed shared entries — so they are not affected.
        use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
        let agent = new_vault();
        let store = LayeredUserVault::new(agent, None);
        for tier in [VaultSource::AgentPerUser, VaultSource::GlobalPerUser] {
            let err =
                workforce_agent::runner::CURRENT_USER_ID.sync_scope("kings".to_string(), || {
                    store
                        .set_at_tier(
                            "users/kings/skills/jira/api_token",
                            "v",
                            tier,
                            VaultActor::Cli,
                        )
                        .unwrap_err()
                });
            assert!(
                err.to_string().contains("already includes a `users/"),
                "tier {:?} did not reject double-prefix path: {err}",
                tier
            );
        }
    }

    #[tokio::test]
    async fn list_at_tier_returns_paths_grouped_by_tier() {
        use workforce_common::settings::{SecretStore, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("users/kings/skills/jira/api_token", "agent-per-user")
            .unwrap();
        global
            .set("users/kings/skills/jira/email", "global-per-user")
            .unwrap();
        agent
            .set("skills/jira/default_workspace", "agent-specific")
            .unwrap();
        global.set("mcp/v1/hubspot/client_id", "global").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));

        let probe = CURRENT_USER_ID
            .scope("kings".to_string(), async {
                (
                    store.list_at_tier(None, VaultSource::AgentPerUser),
                    store.list_at_tier(None, VaultSource::GlobalPerUser),
                    store.list_at_tier(None, VaultSource::AgentSpecific),
                    store.list_at_tier(None, VaultSource::Global),
                )
            })
            .await;
        let (t1, t2, t3, t4) = probe;
        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].0, "skills/jira/api_token");
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].0, "skills/jira/email");
        assert_eq!(t3.len(), 1);
        assert_eq!(t3[0].0, "skills/jira/default_workspace");
        assert_eq!(t4.len(), 1);
        assert_eq!(t4[0].0, "mcp/v1/hubspot/client_id");
    }

    #[tokio::test]
    async fn delete_at_tier_removes_only_named_tier() {
        use workforce_common::settings::{SecretStore, VaultActor, VaultSource};
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("kings".to_string(), async {
                store
                    .set_at_tier(
                        "k",
                        "t2",
                        VaultSource::GlobalPerUser,
                        VaultActor::User("kings"),
                    )
                    .unwrap();
                store
                    .set_at_tier("k", "t3", VaultSource::AgentSpecific, VaultActor::Cli)
                    .unwrap();
                store
                    .set_at_tier("k", "t4", VaultSource::Global, VaultActor::Cli)
                    .unwrap();
                assert!(store
                    .delete_at_tier("k", VaultSource::AgentSpecific)
                    .unwrap());
            })
            .await;

        assert_eq!(global.get("users/kings/k").as_deref(), Some("t2"));
        assert_eq!(agent.get("k"), None);
        assert_eq!(global.get("k").as_deref(), Some("t4"));
    }

    #[tokio::test]
    async fn inspect_returns_empty_when_path_missing() {
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(agent, Some(global));
        assert!(store.inspect("missing").is_empty());
    }

    #[tokio::test]
    async fn github_pat_prefers_workspace_path_over_bare() {
        let agent = new_vault();
        agent
            .set("skills/github/workspaces/contoso/token", "ws-pat")
            .unwrap();
        agent.set("skills/github/token", "bare-pat").unwrap();
        let store = LayeredUserVault::new(agent, None);
        assert_eq!(store.get_github_pat("contoso").as_deref(), Some("ws-pat"));
    }

    #[tokio::test]
    async fn github_pat_falls_back_to_bare_when_workspace_missing() {
        let agent = new_vault();
        agent.set("skills/github/token", "bare-pat").unwrap();
        let store = LayeredUserVault::new(agent, None);
        assert_eq!(store.get_github_pat("contoso").as_deref(), Some("bare-pat"));
    }

    #[tokio::test]
    async fn github_pat_user_override_wins_over_shared_workspace() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/github/workspaces/contoso/token", "shared-ws")
            .unwrap();
        global
            .set(
                "users/kings/skills/github/workspaces/contoso/token",
                "kings-personal",
            )
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get_github_pat("contoso").as_deref(),
                    Some("kings-personal")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn github_pat_returns_none_when_neither_path_set() {
        let agent = new_vault();
        let global = new_vault();
        let store = LayeredUserVault::new(agent, Some(global));
        assert!(store.get_github_pat("contoso").is_none());
    }

    #[tokio::test]
    async fn github_pat_user_flat_override_wins_over_agent_workspace() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/github/workspaces/contoso/token", "shared-ws")
            .unwrap();
        global
            .set("users/kings/skills/github/token", "kings-flat")
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get_github_pat("contoso").as_deref(),
                    Some("kings-flat")
                );
            })
            .await;
    }

    #[test]
    fn resolve_github_pat_prefers_agent_workspace_over_global() {
        let agent = new_vault();
        let global = new_vault();
        agent
            .set("skills/github/workspaces/contoso/token", "agent-ws")
            .unwrap();
        global
            .set("skills/github/workspaces/contoso/token", "global-ws")
            .unwrap();
        assert_eq!(
            resolve_github_pat(Some(&agent), Some(&global), "contoso").as_deref(),
            Some("agent-ws")
        );
    }

    #[test]
    fn resolve_github_pat_prefers_workspace_over_bare_within_a_vault() {
        let agent = new_vault();
        agent
            .set("skills/github/workspaces/contoso/token", "ws-pat")
            .unwrap();
        agent.set("skills/github/token", "bare-pat").unwrap();
        assert_eq!(
            resolve_github_pat(Some(&agent), None, "contoso").as_deref(),
            Some("ws-pat")
        );
    }

    #[test]
    fn resolve_github_pat_falls_through_to_global_bare_when_agent_empty() {
        let agent = new_vault();
        let global = new_vault();
        global.set("skills/github/token", "global-bare").unwrap();
        assert_eq!(
            resolve_github_pat(Some(&agent), Some(&global), "contoso").as_deref(),
            Some("global-bare")
        );
    }

    #[test]
    fn resolve_github_pat_skips_agent_layer_when_none() {
        let global = new_vault();
        global.set("skills/github/token", "global-bare").unwrap();
        assert_eq!(
            resolve_github_pat(None, Some(&global), "acme").as_deref(),
            Some("global-bare")
        );
    }

    #[test]
    fn resolve_github_pat_returns_none_when_nothing_configured() {
        let agent = new_vault();
        let global = new_vault();
        assert!(resolve_github_pat(Some(&agent), Some(&global), "acme").is_none());
    }

    // ─── LayeredUserVault::get_github_pat ────────────────────────────────────
    //
    // Mirrors the github.clone resolution policy but with per-user layering on
    // top. Both `wasm_skill.install` and `github.clone` rely on this so a
    // workspace-scoped PAT (`skills/github/workspaces/{owner}/token`) reaches
    // both tools — the original failure mode was wasm_skill.install seeing
    // only the bare flat path while github.clone saw the workspace one.

    fn new_vault_layered() -> std::sync::Arc<crate::vault::Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        std::sync::Arc::new(crate::vault::Vault::open(&path, "test-key").unwrap())
    }

    #[tokio::test]
    async fn bench_pat_reads_the_flat_token_and_honours_user_overrides() {
        let agent = new_vault_layered();
        let global = new_vault_layered();
        // A bench has no owner to scope by, so an org-scoped token must NOT
        // satisfy it — silently borrowing one org's PAT for any clone would be
        // the wrong credential, not a convenience.
        agent
            .set("skills/github/workspaces/driftwerk/token", "org-only")
            .unwrap();
        assert_eq!(resolve_bench_github_pat(Some(&agent), Some(&global)), None);

        agent.set("skills/github/token", "shared-flat").unwrap();
        assert_eq!(
            resolve_bench_github_pat(Some(&agent), Some(&global)).as_deref(),
            Some("shared-flat")
        );

        global
            .set("users/kings/skills/github/token", "kings-flat")
            .unwrap();
        let (a, g) = (agent.clone(), global.clone());
        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    resolve_bench_github_pat(Some(&a), Some(&g)).as_deref(),
                    Some("kings-flat")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_github_pat_workspace_scoped_resolves_when_only_workspace_set() {
        // Repro of the user-reported failure: a per-org workspace PAT is the
        // only thing configured. Old wasm_skill.install (which called the
        // unscoped `get`) returned None and broke private cargo git fetches.
        let agent = new_vault_layered();
        let global = new_vault_layered();
        agent
            .set(
                "skills/github/workspaces/driftwerk/token",
                "agent-driftwerk",
            )
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        assert_eq!(
            store.get_github_pat("driftwerk").as_deref(),
            Some("agent-driftwerk")
        );
    }

    #[tokio::test]
    async fn get_github_pat_user_workspace_wins_over_shared_workspace() {
        let agent = new_vault_layered();
        let global = new_vault_layered();
        agent
            .set("skills/github/workspaces/driftwerk/token", "shared")
            .unwrap();
        global
            .set(
                "users/kings/skills/github/workspaces/driftwerk/token",
                "kings",
            )
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(store.get_github_pat("driftwerk").as_deref(), Some("kings"));
            })
            .await;
    }

    #[tokio::test]
    async fn get_github_pat_user_flat_wins_over_shared_workspace() {
        // Cross-form precedence: a user-flat override should beat the shared
        // workspace value, otherwise the user can't roll their own PAT once
        // the org has set a workspace-scoped one.
        let agent = new_vault_layered();
        let global = new_vault_layered();
        agent
            .set("skills/github/workspaces/driftwerk/token", "shared-ws")
            .unwrap();
        global
            .set("users/kings/skills/github/token", "kings-flat")
            .unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        CURRENT_USER_ID
            .scope("kings".to_string(), async move {
                assert_eq!(
                    store.get_github_pat("driftwerk").as_deref(),
                    Some("kings-flat")
                );
            })
            .await;
    }

    #[tokio::test]
    async fn get_github_pat_falls_through_to_flat_when_workspace_unset() {
        let agent = new_vault_layered();
        let global = new_vault_layered();
        agent.set("skills/github/token", "shared-flat").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        assert_eq!(
            store.get_github_pat("driftwerk").as_deref(),
            Some("shared-flat")
        );
    }

    #[tokio::test]
    async fn get_github_pat_returns_none_when_nothing_configured() {
        let agent = new_vault_layered();
        let global = new_vault_layered();
        let store = LayeredUserVault::new(agent, Some(global));
        assert!(store.get_github_pat("driftwerk").is_none());
    }
}

/// Resolution-precedence matrix across the four vault tiers — the executable
/// spec behind `docs/VAULT.md` "Layered Resolution" and the
/// `end-user-docs` vault scoping section. A credential at logical path `P` can
/// live in four physical slots; these tests pin which one wins for every
/// combination, plus the one intentional divergence between the skill path and
/// the raw `get` path.
#[cfg(test)]
mod scoping_matrix_tests {
    use super::*;
    use std::sync::Arc;
    use workforce_agent::runner::CURRENT_USER_ID;

    const P: &str = "skills/demo/token";
    const WS: &str = "skills/demo/workspaces/acme/token";
    const UID: &str = "u1";

    fn vault() -> Arc<Vault> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vault.enc");
        std::mem::forget(tmp);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    /// Tier labels: `1` per-user·global, `2` per-user·agent, `3` agent-shared,
    /// `4` global-shared. Writes `val` at `path` in the slot for `tier`.
    fn set_tier(agent: &Vault, global: &Vault, path: &str, tier: char, val: &str) {
        match tier {
            '1' => global.set(&format!("users/{UID}/{path}"), val).unwrap(),
            '2' => agent.set(&format!("users/{UID}/{path}"), val).unwrap(),
            '3' => agent.set(path, val).unwrap(),
            '4' => global.set(path, val).unwrap(),
            _ => unreachable!("unknown tier {tier}"),
        }
    }

    /// Skill/flat credential precedence is strict:
    /// `1` per-user·global > `2` per-user·agent > `3` agent-shared > `4` global-shared.
    /// Exhaustively assert the winner for all 15 non-empty slot combinations.
    /// (`resolve_flat` is equivalent to the skill resolver
    /// `resolve_workspace_or_flat` with a single form — see the cross-form test.)
    #[tokio::test]
    async fn flat_precedence_is_strict_across_all_combinations() {
        const ORDER: [char; 4] = ['1', '2', '3', '4'];
        for mask in 1u8..16 {
            let present: Vec<char> = ORDER
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1u8 << i) != 0)
                .map(|(_, &c)| c)
                .collect();
            let agent = vault();
            let global = vault();
            for &t in &present {
                set_tier(&agent, &global, P, t, &format!("tier-{t}"));
            }
            let expected = *ORDER.iter().find(|c| present.contains(c)).unwrap();
            let got = CURRENT_USER_ID
                .scope(UID.to_string(), async {
                    resolve_flat(Some(&agent), Some(&global), P)
                })
                .await;
            assert_eq!(
                got.as_deref(),
                Some(format!("tier-{expected}").as_str()),
                "combination {present:?} should resolve tier {expected}"
            );
        }
    }

    /// The raw `get_layered` path orders the per-user tiers the OTHER way —
    /// agent-per-user (`2`) before global-per-user (`1`). This divergence is
    /// intentional (MCP PatAuth and capability checks read raw `get`); pin both
    /// orderings so neither is "fixed" into the other by mistake.
    #[tokio::test]
    async fn raw_get_and_skill_path_diverge_on_per_user_order() {
        let agent = vault();
        let global = vault();
        set_tier(&agent, &global, P, '1', "tier-1"); // per-user·global
        set_tier(&agent, &global, P, '2', "tier-2"); // per-user·agent

        let raw = CURRENT_USER_ID
            .scope(UID.to_string(), async {
                get_layered(Some(&agent), Some(&global), P)
            })
            .await;
        assert_eq!(
            raw.as_deref(),
            Some("tier-2"),
            "raw get: agent-per-user wins"
        );

        let skill = CURRENT_USER_ID
            .scope(UID.to_string(), async {
                resolve_flat(Some(&agent), Some(&global), P)
            })
            .await;
        assert_eq!(
            skill.as_deref(),
            Some("tier-1"),
            "skill path: global-per-user wins"
        );
    }

    /// With no identity in scope the per-user tiers vanish; only the shared
    /// tiers resolve (agent-shared > global-shared).
    #[tokio::test]
    async fn no_identity_sees_only_shared_tiers() {
        let agent = vault();
        let global = vault();
        for (t, v) in [
            ('1', "tier-1"),
            ('2', "tier-2"),
            ('3', "tier-3"),
            ('4', "tier-4"),
        ] {
            set_tier(&agent, &global, P, t, v);
        }
        // No CURRENT_USER_ID scope: per-user slots 1 and 2 are invisible.
        assert_eq!(
            resolve_flat(Some(&agent), Some(&global), P).as_deref(),
            Some("tier-3"),
            "no identity → agent-shared wins over global-shared"
        );

        let agent2 = vault();
        let global2 = vault();
        set_tier(&agent2, &global2, P, '4', "tier-4");
        assert_eq!(
            resolve_flat(Some(&agent2), Some(&global2), P).as_deref(),
            Some("tier-4"),
            "global-shared is the last resort"
        );
    }

    /// Cross-form precedence via the real skill resolver: per-user beats shared
    /// ACROSS forms (a per-user flat override can't be masked by an agent-vault
    /// workspace value), and within one tier the workspace form wins.
    #[tokio::test]
    async fn skill_resolver_cross_form_precedence() {
        use workforce_skills::adapters::auth::resolve_workspace_or_flat;

        // per-user flat (slot 1) beats an agent-shared workspace (slot 3, ws form).
        let agent = vault();
        let global = vault();
        global
            .set(&format!("users/{UID}/{P}"), "flat-user-global")
            .unwrap();
        agent.set(WS, "ws-agent-shared").unwrap();
        let store = LayeredUserVault::new(agent, Some(global));
        let got = CURRENT_USER_ID
            .scope(UID.to_string(), async {
                resolve_workspace_or_flat(&store, WS, P)
            })
            .await;
        assert_eq!(
            got.as_deref(),
            Some("flat-user-global"),
            "per-user flat must not be shadowed by a shared workspace value"
        );

        // within the same (agent-shared) tier, the workspace form wins.
        let agent2 = vault();
        agent2.set(WS, "ws-agent").unwrap();
        agent2.set(P, "flat-agent").unwrap();
        let store2 = LayeredUserVault::new(agent2, None);
        assert_eq!(
            resolve_workspace_or_flat(&store2, WS, P).as_deref(),
            Some("ws-agent"),
            "workspace form wins over flat within a tier"
        );
    }

    #[tokio::test]
    async fn billing_identity_prefers_billing_local_then_user() {
        use workforce_agent::runner::{CURRENT_BILLING_IDENTITY, CURRENT_USER_ID};

        // No locals: both None.
        assert_eq!(current_billing_identity_id(), None);

        CURRENT_USER_ID
            .scope("human-uuid".to_string(), async {
                assert_eq!(current_billing_identity_id().as_deref(), Some("human-uuid"));

                CURRENT_BILLING_IDENTITY
                    .scope("system:indexing".to_string(), async {
                        assert_eq!(
                            current_billing_identity_id().as_deref(),
                            Some("system:indexing"),
                            "explicit billing lane wins"
                        );
                        assert_eq!(
                            current_identity_id().as_deref(),
                            Some("human-uuid"),
                            "access identity untouched"
                        );
                    })
                    .await;

                CURRENT_BILLING_IDENTITY
                    .scope(String::new(), async {
                        assert_eq!(
                            current_billing_identity_id().as_deref(),
                            Some("human-uuid"),
                            "empty billing scope falls back"
                        );
                    })
                    .await;
            })
            .await;
    }
}

#[cfg(test)]
mod user_aware_tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_common::settings::SecretStore;

    fn vault() -> Arc<Vault> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.enc");
        std::mem::forget(dir);
        Arc::new(Vault::open(&path, "test-key").unwrap())
    }

    #[test]
    fn a_users_writes_land_in_their_own_namespace() {
        let vault = vault();
        let store = UserAwareVault::for_user(Arc::clone(&vault), "u1");

        store
            .set_with_actor(
                "mcp/v1/hubspot/access_token",
                "at",
                VaultActor::System("oauth_refresh"),
            )
            .unwrap();
        store
            .set_with_actor(
                "users/u1/skills/google/refresh_token",
                "rt",
                VaultActor::Cli,
            )
            .unwrap();

        assert_eq!(
            vault.get("users/u1/mcp/v1/hubspot/access_token").as_deref(),
            Some("at")
        );
        assert!(vault.get("mcp/v1/hubspot/access_token").is_none());
        assert_eq!(
            vault.get("users/u1/skills/google/refresh_token").as_deref(),
            Some("rt")
        );
        assert!(store.delete("mcp/v1/hubspot/access_token").unwrap());
        assert!(vault.get("users/u1/mcp/v1/hubspot/access_token").is_none());
    }

    #[test]
    fn a_batch_lands_in_the_users_namespace() {
        let vault = vault();
        UserAwareVault::for_user(Arc::clone(&vault), "u1")
            .set_many(
                &[("t/a", "1"), ("t/b", "2")],
                VaultActor::System("oauth_refresh"),
            )
            .unwrap();
        assert_eq!(vault.list(None), vec!["users/u1/t/a", "users/u1/t/b"]);
    }

    #[test]
    fn a_strict_view_never_hands_out_a_shared_token() {
        let vault = vault();
        vault
            .set("mcp/v1/hubspot/access_token", "someone-elses")
            .unwrap();

        let strict = UserAwareVault::for_user_strict(Arc::clone(&vault), "u2");
        assert!(strict.get("mcp/v1/hubspot/access_token").is_none());
        assert!(strict.list(None).is_empty());
        assert!(UserAwareVault::for_user(Arc::clone(&vault), "u2")
            .get("mcp/v1/hubspot/access_token")
            .is_some());

        strict.set("mcp/v1/hubspot/access_token", "mine").unwrap();
        assert_eq!(
            strict.get("mcp/v1/hubspot/access_token").as_deref(),
            Some("mine")
        );
    }

    #[test]
    fn a_view_never_reaches_another_users_entries() {
        let vault = vault();
        let theirs = "users/u1/mcp/v1/hubspot/access_token";
        vault.set(theirs, "u1-token").unwrap();

        for view in [
            UserAwareVault::for_user(Arc::clone(&vault), "u2"),
            UserAwareVault::for_user_strict(Arc::clone(&vault), "u2"),
        ] {
            assert!(view.get(theirs).is_none());
            assert!(view.get_entry(theirs).is_none());
            view.set(theirs, "u2-token").unwrap();
            assert!(view.delete(theirs).unwrap());
        }
        assert_eq!(vault.get(theirs).as_deref(), Some("u1-token"));
        assert_eq!(vault.list(None), vec![theirs]);
    }

    #[test]
    fn without_an_identity_paths_are_used_as_given() {
        let vault = vault();
        UserAwareVault::global(Arc::clone(&vault))
            .set("mcp/v1/linear/access_token", "at")
            .unwrap();
        assert_eq!(
            vault.get("mcp/v1/linear/access_token").as_deref(),
            Some("at")
        );
    }

    #[test]
    fn refresh_locks_key_on_where_the_write_lands() {
        let vault = vault();
        let (u1, u2) = (
            UserAwareVault::for_user(Arc::clone(&vault), "u1"),
            UserAwareVault::for_user(Arc::clone(&vault), "u2"),
        );
        assert_ne!(u1.write_location("t"), u2.write_location("t"));
        assert_eq!(u1.write_location("t"), vault.write_location("users/u1/t"));
    }

    #[tokio::test]
    async fn a_layered_batch_commits_to_the_default_write_tier() {
        let (agent, global) = (vault(), vault());
        let layered = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));

        CURRENT_USER_ID
            .scope("u1".to_string(), async {
                layered
                    .set_many(
                        &[
                            ("skills/x/refresh_token", "rt"),
                            ("skills/x/access_token", "at"),
                        ],
                        VaultActor::System("oauth_refresh"),
                    )
                    .unwrap();
                assert_eq!(
                    global.get("users/u1/skills/x/refresh_token").as_deref(),
                    Some("rt")
                );
                assert_eq!(
                    layered.write_location("skills/x/refresh_token"),
                    global.write_location("users/u1/skills/x/refresh_token")
                );
            })
            .await;

        layered
            .set_many(&[("engagement/overrides", "{}")], VaultActor::System("job"))
            .unwrap();
        assert_eq!(agent.get("engagement/overrides").as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn a_tier_move_commits_across_vaults_and_never_eats_an_aliased_tier() {
        use workforce_common::settings::VaultSource;
        let (agent, global) = (vault(), vault());
        let layered = LayeredUserVault::new(Arc::clone(&agent), Some(Arc::clone(&global)));
        agent.set("users/u1/skills/x/token", "tok").unwrap();

        CURRENT_USER_ID
            .scope("u1".to_string(), async {
                layered
                    .move_at_tier(
                        ("skills/x/token", VaultSource::AgentPerUser),
                        ("skills/x/token", VaultSource::GlobalPerUser),
                        "tok",
                        VaultActor::User("u1"),
                    )
                    .unwrap();
            })
            .await;
        assert!(agent.get("users/u1/skills/x/token").is_none());
        assert_eq!(
            global.get("users/u1/skills/x/token").as_deref(),
            Some("tok")
        );

        let single = LayeredUserVault::new(Arc::clone(&agent), None);
        agent.set("skills/y/token", "keep").unwrap();
        single
            .move_at_tier(
                ("skills/y/token", VaultSource::AgentSpecific),
                ("skills/y/token", VaultSource::Global),
                "keep",
                VaultActor::Cli,
            )
            .unwrap();
        assert_eq!(agent.get("skills/y/token").as_deref(), Some("keep"));
    }
}
