//! Mandatory LLM spend enforcement (`[budget]`).
//!
//! One admission check runs before every LLM turn — `process_message` and
//! `run_isolated` both funnel through [`CostGovernor::admit_turn`] — so chat,
//! scheduler jobs, heartbeats, monitors, workflows and curation are all
//! covered at a single choke point. Base ceilings live only in config files
//! (`[budget]` is outside `OVERLAYABLE_PREFIXES`, so no runtime writer can
//! touch it); TOML per-agent entries can only tighten the hub-wide values.
//! SysAdmin-granted runtime overrides ([`crate::budget_overrides`]) REPLACE
//! a scope's ceiling — in either direction — and are consulted live.
//! Budgets count money: only metered spend. A subscription call's estimated
//! value stays in the ledger but never consumes a budget.
//!
//! The governor fails OPEN on telemetry-store errors: a broken store must
//! never take down chat, and the failure is logged.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use std::sync::Arc;
use tracing::warn;
use workforce_common::persistence::MeteredSpend;
use workforce_common::settings::BudgetConfig;
use workforce_common::telemetry::TelemetryStore;

use crate::hub_core::budget_overrides::{BudgetOverride, BudgetOverrideStore, OverrideScope};

const SPEND_CACHE_TTL: Duration = Duration::from_secs(30);

/// How long a fetched override set is trusted before re-reading the store —
/// the ceiling on how stale a freshly granted boost can be.
const OVERRIDE_CACHE_TTL: Duration = Duration::from_secs(5);

/// Verdict for one prospective LLM turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Admission {
    Proceed,
    /// Over the soft-warn ratio for a scope; emitted once per scope per day.
    Warn(String),
    /// A ceiling is reached; the turn must not run.
    Deny(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Scope {
    Hub,
    Agent(String),
    Identity(String),
}

impl Scope {
    fn describe(&self) -> String {
        match self {
            Scope::Hub => "this workspace".to_string(),
            Scope::Agent(id) => format!("agent `{id}`"),
            Scope::Identity(_) => "your account".to_string(),
        }
    }

    fn config_key(&self) -> &'static str {
        match self {
            Scope::Hub => "hub_daily_usd",
            Scope::Agent(_) => "agent_daily_usd",
            Scope::Identity(_) => "identity_daily_usd",
        }
    }

    /// The part of `spend` this scope's ceiling counts.
    fn spent_in(&self, spend: &[MeteredSpend]) -> f64 {
        spend
            .iter()
            .filter(|s| match self {
                Scope::Hub => true,
                Scope::Agent(id) => s.agent_id == *id,
                Scope::Identity(id) => s.identity_id.as_deref() == Some(id.as_str()),
            })
            .map(|s| s.cost_usd)
            .sum()
    }
}

/// How status reports spend recorded under no identity.
const UNKNOWN_IDENTITY: &str = "unknown";

/// `spend` totalled per `key`.
fn total_by(spend: &[MeteredSpend], key: impl Fn(&MeteredSpend) -> String) -> HashMap<String, f64> {
    spend.iter().fold(HashMap::new(), |mut totals, s| {
        *totals.entry(key(s)).or_default() += s.cost_usd;
        totals
    })
}

pub struct CostGovernor {
    store: Arc<dyn TelemetryStore>,
    cfg: BudgetConfig,
    tz: Tz,
    overrides: Option<Arc<dyn BudgetOverrideStore>>,
    override_cache: Mutex<Option<(Instant, Vec<BudgetOverride>)>>,
    spend: Mutex<HashMap<Scope, (Instant, f64)>>,
    warned: Mutex<HashSet<(Scope, NaiveDate)>>,
}

impl CostGovernor {
    /// Standard construction: picks up the process-wide override store
    /// installed at boot (None in most tests → base config only).
    pub fn new(store: Arc<dyn TelemetryStore>, cfg: BudgetConfig, tz: Tz) -> Self {
        Self::with_override_store(
            store,
            cfg,
            tz,
            crate::hub_core::budget_overrides::global_store(),
        )
    }

    pub fn with_override_store(
        store: Arc<dyn TelemetryStore>,
        cfg: BudgetConfig,
        tz: Tz,
        overrides: Option<Arc<dyn BudgetOverrideStore>>,
    ) -> Self {
        // Once per process, not once per agent service.
        static DEAD_OVERRIDE_WARNING: std::sync::Once = std::sync::Once::new();
        DEAD_OVERRIDE_WARNING.call_once(|| warn_dead_toml_overrides(&cfg));
        Self {
            store,
            cfg,
            tz,
            overrides,
            override_cache: Mutex::new(None),
            spend: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
        }
    }

    pub fn config(&self) -> &BudgetConfig {
        &self.cfg
    }

    /// The overrides currently in force, refreshed from the store at most
    /// every [`OVERRIDE_CACHE_TTL`]. The store read happens OUTSIDE the cache
    /// lock (it can be a network round-trip; holding the lock would park
    /// every concurrent turn behind it), and an error re-stamps the cache so
    /// a dead store is retried at TTL cadence — fail-open to the last known
    /// set, never a per-turn stall.
    fn active_overrides(&self) -> Vec<BudgetOverride> {
        let Some(ref store) = self.overrides else {
            return Vec::new();
        };
        {
            let cache = self.override_cache.lock().expect("override cache lock");
            if let Some((at, ref cached)) = *cache {
                if at.elapsed() < OVERRIDE_CACHE_TTL {
                    return cached.clone();
                }
            }
        }
        let result = store.list_active(Utc::now());
        let mut cache = self.override_cache.lock().expect("override cache lock");
        match result {
            Ok(fresh) => {
                *cache = Some((Instant::now(), fresh.clone()));
                fresh
            }
            Err(e) => {
                warn!(error = %e, "Budget override lookup failed — using last known set");
                let stale = cache.as_ref().map(|(_, c)| c.clone()).unwrap_or_default();
                *cache = Some((Instant::now(), stale.clone()));
                stale
            }
        }
    }

    /// The runtime override in force for a scope, if any.
    fn find_override<'a>(
        scope: &Scope,
        active: &'a [BudgetOverride],
    ) -> Option<&'a BudgetOverride> {
        active.iter().find(|o| match (o.scope, scope) {
            (OverrideScope::Hub, Scope::Hub) => true,
            (OverrideScope::Agent, Scope::Agent(id)) => o.key == *id,
            (OverrideScope::Identity, Scope::Identity(id)) => o.key == *id,
            _ => false,
        })
    }

    /// Whether `identity`'s spend is exempt from the per-identity ceiling.
    pub fn is_exempt(&self, identity: &str) -> bool {
        self.cfg.exempt_identities.iter().any(|e| e == identity)
    }

    /// Admit or refuse one LLM turn for `agent_id`, attributed to `identity`.
    /// The first breached ceiling wins; warnings are collected only when
    /// nothing denies.
    pub fn admit_turn(&self, agent_id: &str, identity: Option<&str>) -> Admission {
        if !self.cfg.enabled {
            return Admission::Proceed;
        }
        let (day, from, to) = self.today_window();
        let effective = self.cfg.for_agent(agent_id);
        let active = self.active_overrides();

        let mut checks = vec![
            (Scope::Hub, self.cfg.hub_daily_usd),
            (
                Scope::Agent(agent_id.to_string()),
                effective.agent_daily_usd,
            ),
        ];
        if let Some(id) = identity.filter(|id| !self.is_exempt(id)) {
            checks.push((Scope::Identity(id.to_string()), self.cfg.identity_daily_usd));
        }
        for (scope, limit) in &mut checks {
            if let Some(replacement) = Self::find_override(scope, &active) {
                *limit = replacement.daily_usd;
            }
        }

        let mut warning = None;
        for (scope, limit) in checks {
            if limit <= 0.0 {
                continue;
            }
            let spent = match self.spent_today(&scope, from, to) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, scope = ?scope, "Budget spend lookup failed — admitting turn");
                    continue;
                }
            };
            if spent >= limit {
                return Admission::Deny(deny_message(&scope, spent, limit));
            }
            if spent >= limit * self.cfg.soft_warn_ratio && self.first_warning_today(&scope, day) {
                warning.get_or_insert_with(|| warn_message(&scope, spent, limit));
            }
        }
        warning.map_or(Admission::Proceed, Admission::Warn)
    }

    /// (local calendar day, UTC window start, UTC now).
    fn today_window(&self) -> (NaiveDate, DateTime<Utc>, DateTime<Utc>) {
        let now = Utc::now();
        let local = now.with_timezone(&self.tz);
        let day = local.date_naive();
        let midnight = day.and_hms_opt(0, 0, 0).expect("midnight exists");
        let from = self
            .tz
            .from_local_datetime(&midnight)
            .earliest()
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|| now - chrono::Duration::days(1));
        (day, from, now)
    }

    fn spent_today(
        &self,
        scope: &Scope,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> eyre::Result<f64> {
        if let Some((at, cached)) = self.spend.lock().expect("spend lock").get(scope) {
            if at.elapsed() < SPEND_CACHE_TTL {
                return Ok(*cached);
            }
        }
        let spent = scope.spent_in(&self.store.metered_spend(from, to)?);
        self.spend
            .lock()
            .expect("spend lock")
            .insert(scope.clone(), (Instant::now(), spent));
        Ok(spent)
    }

    fn first_warning_today(&self, scope: &Scope, day: NaiveDate) -> bool {
        let mut warned = self.warned.lock().expect("warned lock");
        warned.retain(|(_, d)| *d == day);
        warned.insert((scope.clone(), day))
    }

    /// A live snapshot of spend vs. effective ceilings. `agent_ids` names the
    /// agents to report (the registry's list); identity coverage is either
    /// one identity (the caller's own) or — for admins — every identity with
    /// spend today. Spend figures are direct store reads (no per-scope
    /// cache), so this reflects "now" to within telemetry-write latency.
    pub fn status(
        &self,
        agent_ids: &[String],
        identity: Option<&str>,
        all_identities: bool,
    ) -> eyre::Result<BudgetStatus> {
        let (day, from, to) = self.today_window();
        let active = self.active_overrides();
        let spend = self.store.metered_spend(from, to)?;
        let by_agent = total_by(&spend, |s| s.agent_id.clone());
        let by_identity: Vec<(String, f64)> = total_by(&spend, |s| {
            s.identity_id
                .clone()
                .unwrap_or_else(|| UNKNOWN_IDENTITY.to_string())
        })
        .into_iter()
        .collect();

        let scope_status = |scope: Scope, base_limit: f64, spent: f64| {
            let ov = Self::find_override(&scope, &active);
            // An exempt identity has no ceiling at all — reporting the base
            // limit would show a false red bar for the busiest system users.
            let exempt = matches!(&scope, Scope::Identity(id) if self.is_exempt(id));
            let limit = if exempt {
                0.0
            } else {
                ov.map(|o| o.daily_usd).unwrap_or(base_limit)
            };
            ScopeStatus {
                key: match &scope {
                    Scope::Hub => String::new(),
                    Scope::Agent(id) | Scope::Identity(id) => id.clone(),
                },
                spent_usd: spent,
                limit_usd: limit,
                unlimited: limit <= 0.0,
                overridden: !exempt && ov.is_some(),
                override_expires_at: ov.filter(|_| !exempt).and_then(|o| o.expires_at),
            }
        };

        let hub = scope_status(
            Scope::Hub,
            self.cfg.hub_daily_usd,
            Scope::Hub.spent_in(&spend),
        );

        // Agents: the registry's list, plus any agent-scoped override target
        // not in it (a departed agent's override must stay visible/clearable).
        let mut agent_keys: Vec<String> = agent_ids.to_vec();
        for o in active.iter().filter(|o| o.scope == OverrideScope::Agent) {
            if !agent_keys.contains(&o.key) {
                agent_keys.push(o.key.clone());
            }
        }
        let mut agents: Vec<ScopeStatus> = agent_keys
            .iter()
            .map(|id| {
                scope_status(
                    Scope::Agent(id.clone()),
                    self.cfg.for_agent(id).agent_daily_usd,
                    by_agent.get(id).copied().unwrap_or(0.0),
                )
            })
            .collect();
        agents.sort_by(|a, b| b.spent_usd.total_cmp(&a.spent_usd));

        let mut identities: Vec<ScopeStatus> = if all_identities {
            // Everyone with spend today, plus override targets with no spend
            // yet — a proactively granted boost must be visible before its
            // first turn.
            let mut keys: Vec<(String, f64)> = by_identity.clone();
            for o in active.iter().filter(|o| o.scope == OverrideScope::Identity) {
                if !keys.iter().any(|(id, _)| *id == o.key) {
                    keys.push((o.key.clone(), 0.0));
                }
            }
            keys.into_iter()
                .map(|(id, spent)| {
                    scope_status(Scope::Identity(id), self.cfg.identity_daily_usd, spent)
                })
                .collect()
        } else {
            identity
                .map(|id| {
                    let spent = by_identity
                        .iter()
                        .find(|(i, _)| i == id)
                        .map_or(0.0, |(_, c)| *c);
                    vec![scope_status(
                        Scope::Identity(id.to_string()),
                        self.cfg.identity_daily_usd,
                        spent,
                    )]
                })
                .unwrap_or_default()
        };
        identities.sort_by(|a, b| b.spent_usd.total_cmp(&a.spent_usd));

        Ok(BudgetStatus {
            enabled: self.cfg.enabled,
            day,
            hub,
            agents,
            identities,
            overrides: active,
        })
    }
}

/// Spend vs. effective ceiling for one scope. `limit_usd <= 0` (mirrored by
/// `unlimited`) means no ceiling.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScopeStatus {
    pub key: String,
    pub spent_usd: f64,
    pub limit_usd: f64,
    pub unlimited: bool,
    pub overridden: bool,
    pub override_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BudgetStatus {
    pub enabled: bool,
    pub day: NaiveDate,
    pub hub: ScopeStatus,
    pub agents: Vec<ScopeStatus>,
    pub identities: Vec<ScopeStatus>,
    pub overrides: Vec<BudgetOverride>,
}

/// TOML `[budget.agents.*]` entries can only tighten (min-merge). An entry
/// above its hub-wide value is dead config — the mistake that silently kept
/// an agent capped in production; say so at boot.
fn warn_dead_toml_overrides(cfg: &BudgetConfig) {
    for (agent, o) in &cfg.agents {
        if let Some(v) = o.agent_daily_usd {
            if cfg.agent_daily_usd > 0.0 && v > cfg.agent_daily_usd {
                warn!(
                    agent = %agent,
                    override_usd = v,
                    global_usd = cfg.agent_daily_usd,
                    "[budget.agents] entries only tighten — this one exceeds the global and has no effect"
                );
            }
        }
    }
}

/// The trust-free backstop behind the estimate-confirm flow: a `confirmed:
/// true` scheduler call — the only shape that creates over-threshold
/// recurring work — produces a real user-facing Ask via the existing
/// self-serve approval prompt, so the model cannot approve on the user's
/// behalf. Below-threshold creates never set `confirmed` and never prompt.
/// Mirrors `crate::aws::default_policies`, with the same trade-off:
/// constitutional-first, first-match evaluation means an operator YAML rule
/// naming `scheduler` is shadowed by this rule's outer Allow — operator
/// tightening for the scheduler belongs in the permissions cascade (which
/// runs after this engine) or `[budget]`.
pub fn default_policies(budget: &BudgetConfig) -> Vec<workforce_policy::rules::PolicyRule> {
    use workforce_policy::rules::{
        ConditionOperator, PolicyRule, PolicyScope, RuleAction, RuleCondition,
    };
    if !budget.enabled {
        return Vec::new();
    }
    vec![PolicyRule {
        name: "scheduler-confirmed-create-ask".to_string(),
        scope: PolicyScope::Global,
        tool: "scheduler".to_string(),
        action: RuleAction::Allow,
        conditions: vec![RuleCondition {
            field: "confirmed".to_string(),
            operator: ConditionOperator::Equals,
            value: serde_json::Value::Bool(true),
            action: RuleAction::Ask,
        }],
        priority: 10,
        constitutional: true,
    }]
}

fn deny_message(scope: &Scope, spent: f64, limit: f64) -> String {
    format!(
        "Daily LLM budget reached for {}: ${:.2} of ${:.2} spent today. \
         New requests are paused until tomorrow. A SysAdmin can raise it \
         immediately with a runtime override (ask an agent via the `budget` \
         tool, or use the admin UI's Budget page), or permanently via \
         `[budget] {}` in the workforce config file.",
        scope.describe(),
        spent,
        limit,
        scope.config_key()
    )
}

fn warn_message(scope: &Scope, spent: f64, limit: f64) -> String {
    format!(
        "Heads up: {} has spent ${:.2} of its ${:.2} daily LLM budget.",
        scope.describe(),
        spent,
        limit
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::persistence::{CostRecord, SqliteStore};
    use workforce_common::settings::AgentBudgetOverride;

    fn record(agent: &str, identity: Option<&str>, cost: f64) -> CostRecord {
        CostRecord {
            agent_id: agent.to_string(),
            model: "m".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cost_usd: cost,
            team_id: None,
            task_id: None,
            task_type: None,
            ticket_id: None,
            identity_id: identity.map(str::to_string),
            description: None,
            ..Default::default()
        }
    }

    fn governor(cfg: BudgetConfig, records: &[CostRecord]) -> CostGovernor {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        for r in records {
            store.record_cost(r).unwrap();
        }
        CostGovernor::new(store, cfg, chrono_tz::UTC)
    }

    fn cfg() -> BudgetConfig {
        BudgetConfig {
            hub_daily_usd: 100.0,
            agent_daily_usd: 50.0,
            identity_daily_usd: 10.0,
            ..BudgetConfig::default()
        }
    }

    #[test]
    fn under_all_limits_proceeds() {
        let g = governor(cfg(), &[record("a", Some("u"), 1.0)]);
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    #[test]
    fn identity_over_limit_denies() {
        let g = governor(cfg(), &[record("a", Some("u"), 12.0)]);
        match g.admit_turn("a", Some("u")) {
            Admission::Deny(msg) => {
                assert!(msg.contains("identity_daily_usd"), "{msg}");
                assert!(msg.contains("$12.00"), "{msg}");
            }
            other => panic!("expected deny, got {other:?}"),
        }
    }

    #[test]
    fn agent_over_limit_denies_regardless_of_identity() {
        let g = governor(cfg(), &[record("a", None, 55.0)]);
        assert!(matches!(g.admit_turn("a", Some("u")), Admission::Deny(_)));
        assert_eq!(g.admit_turn("other", Some("u")), Admission::Proceed);
    }

    #[test]
    fn hub_limit_covers_all_agents() {
        let g = governor(
            cfg(),
            &[
                record("a", None, 49.0),
                record("b", None, 49.0),
                record("c", None, 30.0),
            ],
        );
        assert!(matches!(
            g.admit_turn("fresh-agent", None),
            Admission::Deny(_)
        ));
    }

    #[test]
    fn exempt_identity_skips_identity_cap_but_not_hub() {
        let mut c = cfg();
        c.exempt_identities = vec!["system:consolidation".to_string()];
        let g = governor(c, &[record("a", Some("system:consolidation"), 20.0)]);
        assert_eq!(
            g.admit_turn("a", Some("system:consolidation")),
            Admission::Proceed
        );
    }

    #[test]
    fn zero_limit_means_unlimited() {
        let c = BudgetConfig {
            hub_daily_usd: 0.0,
            agent_daily_usd: 0.0,
            identity_daily_usd: 0.0,
            ..BudgetConfig::default()
        };
        let g = governor(c, &[record("a", Some("u"), 99999.0)]);
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    #[test]
    fn disabled_config_admits_everything() {
        let c = BudgetConfig {
            enabled: false,
            ..cfg()
        };
        let g = governor(c, &[record("a", Some("u"), 99999.0)]);
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    #[test]
    fn soft_warning_fires_once_per_day() {
        let g = governor(cfg(), &[record("a", Some("u"), 9.0)]);
        assert!(matches!(g.admit_turn("a", Some("u")), Admission::Warn(_)));
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    #[test]
    fn per_agent_override_tightens() {
        let mut c = cfg();
        c.agents.insert(
            "cheap-agent".to_string(),
            AgentBudgetOverride {
                agent_daily_usd: Some(2.0),
                ..AgentBudgetOverride::default()
            },
        );
        let g = governor(c, &[record("cheap-agent", None, 3.0)]);
        assert!(matches!(
            g.admit_turn("cheap-agent", None),
            Admission::Deny(_)
        ));
    }

    #[test]
    fn tighten_only_semantics() {
        let base = cfg();
        let mut c = base.clone();
        c.agents.insert(
            "a".to_string(),
            AgentBudgetOverride {
                agent_daily_usd: Some(500.0),
                ..AgentBudgetOverride::default()
            },
        );
        assert_eq!(c.for_agent("a").agent_daily_usd, 50.0);
        assert_eq!(c.for_agent("unlisted").agent_daily_usd, 50.0);
    }

    #[test]
    fn subscription_spend_never_consumes_a_budget() {
        let served = |r: CostRecord| CostRecord {
            billing: workforce_common::subscription::Billing::Subscription,
            ..r
        };
        let g = governor(cfg(), &[served(record("a1", Some("alice"), 500.0))]);
        assert_eq!(g.admit_turn("a1", Some("alice")), Admission::Proceed);
        let status = g.status(&["a1".to_string()], Some("alice"), false).unwrap();
        assert_eq!(status.hub.spent_usd, 0.0);
        assert_eq!(status.identities[0].spent_usd, 0.0);

        let g = governor(cfg(), &[record("a1", Some("alice"), 500.0)]);
        assert!(matches!(
            g.admit_turn("a1", Some("alice")),
            Admission::Deny(_)
        ));
    }

    #[test]
    fn empty_store_admits() {
        let g = governor(cfg(), &[]);
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    fn governor_with_overrides(
        cfg: BudgetConfig,
        records: &[CostRecord],
        overrides: &[crate::hub_core::budget_overrides::BudgetOverride],
    ) -> CostGovernor {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        for r in records {
            store.record_cost(r).unwrap();
        }
        let ov = Arc::new(
            crate::hub_core::budget_overrides::sqlite::SqliteBudgetOverrideStore::in_memory()
                .unwrap(),
        );
        for o in overrides {
            use crate::hub_core::budget_overrides::BudgetOverrideStore;
            ov.upsert(o).unwrap();
        }
        CostGovernor::with_override_store(store, cfg, chrono_tz::UTC, Some(ov))
    }

    fn ov(
        scope: crate::hub_core::budget_overrides::OverrideScope,
        key: &str,
        usd: f64,
        expires_at: Option<DateTime<Utc>>,
    ) -> crate::hub_core::budget_overrides::BudgetOverride {
        crate::hub_core::budget_overrides::BudgetOverride {
            scope,
            key: key.to_string(),
            daily_usd: usd,
            expires_at,
            note: None,
            created_by: "admin".to_string(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn identity_override_raises_the_ceiling() {
        use crate::hub_core::budget_overrides::OverrideScope;
        // $12 spent, base identity cap $10 → denied; override to $100 → admitted.
        let g = governor_with_overrides(
            cfg(),
            &[record("a", Some("u"), 12.0)],
            &[ov(OverrideScope::Identity, "u", 100.0, None)],
        );
        assert_eq!(g.admit_turn("a", Some("u")), Admission::Proceed);
    }

    #[test]
    fn agent_override_replaces_rather_than_tightens() {
        use crate::hub_core::budget_overrides::OverrideScope;
        // Base agent cap $50, spent $55 → denied without; runtime override
        // REPLACES (unlike the TOML min-merge) so $500 admits.
        let g = governor_with_overrides(
            cfg(),
            &[record("a", None, 55.0)],
            &[ov(OverrideScope::Agent, "a", 500.0, None)],
        );
        assert_eq!(g.admit_turn("a", None), Admission::Proceed);
    }

    #[test]
    fn hub_override_applies_and_can_lower() {
        use crate::hub_core::budget_overrides::OverrideScope;
        let g = governor_with_overrides(
            cfg(),
            &[record("a", None, 30.0)],
            &[ov(OverrideScope::Hub, "", 20.0, None)],
        );
        assert!(matches!(g.admit_turn("b", None), Admission::Deny(_)));
    }

    #[test]
    fn expired_override_falls_back_to_base() {
        use crate::hub_core::budget_overrides::OverrideScope;
        let g = governor_with_overrides(
            cfg(),
            &[record("a", Some("u"), 12.0)],
            &[ov(
                OverrideScope::Identity,
                "u",
                100.0,
                Some(Utc::now() - chrono::Duration::minutes(1)),
            )],
        );
        assert!(matches!(g.admit_turn("a", Some("u")), Admission::Deny(_)));
    }

    #[test]
    fn status_reports_spend_limits_and_overrides() {
        use crate::hub_core::budget_overrides::OverrideScope;
        let g = governor_with_overrides(
            cfg(),
            &[record("a", Some("u"), 5.0), record("b", Some("v"), 2.0)],
            &[ov(OverrideScope::Identity, "u", 100.0, None)],
        );
        let s = g
            .status(&["a".to_string(), "b".to_string()], None, true)
            .unwrap();
        assert!(s.enabled);
        assert_eq!(s.hub.spent_usd, 7.0);
        assert_eq!(s.hub.limit_usd, 100.0);
        assert_eq!(s.agents.len(), 2);
        assert_eq!(s.agents[0].key, "a", "sorted by spend");
        let u = s.identities.iter().find(|i| i.key == "u").unwrap();
        assert!(u.overridden);
        assert_eq!(u.limit_usd, 100.0);
        let v = s.identities.iter().find(|i| i.key == "v").unwrap();
        assert!(!v.overridden);
        assert_eq!(v.limit_usd, 10.0);
        assert_eq!(s.overrides.len(), 1);

        let own = g.status(&["a".to_string()], Some("v"), false).unwrap();
        assert_eq!(own.identities.len(), 1);
        assert_eq!(own.identities[0].key, "v");
        assert_eq!(own.identities[0].spent_usd, 2.0);
    }

    #[test]
    fn today_window_is_local_midnight_in_the_configured_tz() {
        for tz in [
            chrono_tz::UTC,
            chrono_tz::Asia::Tokyo,
            chrono_tz::US::Hawaii,
        ] {
            let g = CostGovernor::new(Arc::new(SqliteStore::in_memory().unwrap()), cfg(), tz);
            let (day, from, to) = g.today_window();
            assert_eq!(from.with_timezone(&tz).date_naive(), day);
            assert_eq!(
                from.with_timezone(&tz).time(),
                chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            );
            assert!(from <= to);
            assert!(to - from <= chrono::Duration::hours(24) + chrono::Duration::minutes(1));
            assert_eq!(to.with_timezone(&tz).date_naive(), day);
        }
    }

    #[test]
    fn spend_recorded_now_counts_in_every_tz_window() {
        // A record stamped "now" must land inside today's window regardless
        // of the governor's timezone — the incident scope check depends on it.
        for tz in [
            chrono_tz::UTC,
            chrono_tz::Asia::Tokyo,
            chrono_tz::US::Hawaii,
        ] {
            let store = Arc::new(SqliteStore::in_memory().unwrap());
            store.record_cost(&record("a", Some("u"), 12.0)).unwrap();
            let g = CostGovernor::new(store, cfg(), tz);
            assert!(
                matches!(g.admit_turn("a", Some("u")), Admission::Deny(_)),
                "spend must be visible in tz {tz:?}"
            );
        }
    }
}
