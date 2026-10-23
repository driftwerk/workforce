use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::persistence::CostRecord;
use crate::subscription::{Billing, CredentialTier, Origin, Served};
use crate::telemetry::TelemetryStore;

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};

/// Tracks LLM usage costs across agents, teams, and models.
pub struct CostTracker {
    records: RwLock<Vec<UsageRecord>>,
    ledger_keys: Mutex<HashSet<String>>,
    model_pricing: RwLock<HashMap<String, ModelPricing>>,
    store: Option<Arc<dyn TelemetryStore>>,
}

/// A single usage record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub agent_id: String,
    pub team_id: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written to the prompt cache (Anthropic / Gemini / OpenRouter).
    /// Billed at `cache_write_per_million` if set, else `input_per_million`.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Tokens read from the prompt cache. Billed at `cache_read_per_million`
    /// if set, else falls back to `input_per_million`.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
    pub task_id: Option<String>,
    pub task_type: Option<String>,
    pub session_id: Option<String>,
    pub ticket_id: Option<String>,
    /// Canonical identity UUID of the user who triggered this work, when
    /// available. `None` for system-initiated tasks.
    #[serde(default)]
    pub identity_id: Option<String>,
    /// Brief intent of the call (truncated prompt excerpt, "vision:<model>",
    /// "transcription:<seconds>s", etc.) for the cost-attribution UI.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub billing: Billing,
    /// Whose plan paid, for a subscription row.
    #[serde(default)]
    pub payer: Option<String>,
    #[serde(default)]
    pub credential_tier: Option<CredentialTier>,
    /// Identifies the call across replays; see [`CostEntry::ledger_key`].
    #[serde(default)]
    pub ledger_key: Option<String>,
}

/// Token counts of one billable call. `input` is the fresh (non-cached)
/// portion; cache reads and writes are billed at their own rates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// One billable call for [`CostTracker::record`].
#[derive(Debug, Clone)]
pub struct CostEntry<'a> {
    pub agent_id: &'a str,
    /// The model the call ran on; a subscription call is keyed under its
    /// route by [`Served::ledger_model`].
    pub model: &'a str,
    pub tokens: TokenCounts,
    pub team_id: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub task_type: Option<&'a str>,
    pub ticket_id: Option<&'a str>,
    pub identity_id: Option<&'a str>,
    pub description: Option<&'a str>,
    pub origin: Origin,
    /// Present exactly when a subscription paid for the call.
    pub served: Option<Served>,
    /// Identifies a call that may be recorded more than once, such as a
    /// durable coding round collected again after a restart: the ledger
    /// keeps the first record under a key and ignores the rest.
    pub ledger_key: Option<&'a str>,
}

impl<'a> CostEntry<'a> {
    pub fn new(agent_id: &'a str, model: &'a str, tokens: TokenCounts, origin: Origin) -> Self {
        Self {
            agent_id,
            model,
            tokens,
            team_id: None,
            task_id: None,
            task_type: None,
            ticket_id: None,
            identity_id: None,
            description: None,
            origin,
            served: None,
            ledger_key: None,
        }
    }
}

/// Pricing for a model.
///
/// Cache rates are optional. When absent, cache reads + writes fall back to
/// the regular `input_per_million` rate (i.e. caching is reported but not
/// discounted, which is what the codebase did before cache-aware pricing).
///
/// Anthropic 5-minute ephemeral pricing: cache reads cost 0.10× input,
/// cache writes cost 1.25× input. Gemini implicit caching: cache reads
/// cost 0.25× input, no separate write rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    pub model: String,
    /// Cost per 1M input tokens in USD.
    pub input_per_million: f64,
    /// Cost per 1M output tokens in USD.
    pub output_per_million: f64,
    /// Cost per 1M cache-read input tokens. None → defaults to `input_per_million`.
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
    /// Cost per 1M cache-write input tokens. None → defaults to `input_per_million`.
    #[serde(default)]
    pub cache_write_per_million: Option<f64>,
    /// Source of this pricing data: "default", "api_fetch", "manual", "provider_default".
    pub source: Option<String>,
    /// When this pricing was last updated (RFC3339).
    pub updated_at: Option<String>,
}

/// Budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub daily_limit: f64,
    pub monthly_limit: f64,
    pub per_agent_limit: f64,
    pub per_team_limit: f64,
    pub per_session_limit: f64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            daily_limit: 0.0,
            monthly_limit: 0.0,
            per_agent_limit: 0.0,
            per_team_limit: 0.0,
            per_session_limit: 0.0,
        }
    }
}

/// Cost summary for a time period.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    pub total_cost: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_cache_creation_input_tokens: u64,
    #[serde(default)]
    pub total_cache_read_input_tokens: u64,
    pub by_agent: HashMap<String, f64>,
    pub by_model: HashMap<String, f64>,
    pub record_count: usize,
}

/// One bucket in a token-usage time series. `bucket_start` is the UTC midnight
/// of the day the totals belong to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHistoryPoint {
    pub bucket_start: DateTime<Utc>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

// ─── Pricing registry ────────────────────────────────────────────────────────

/// TOML entry for a single model's pricing. `cache_read` and `cache_write`
/// are optional; when omitted, cache tokens are billed at the input rate.
#[derive(Debug, Deserialize)]
struct PricingEntry {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

/// Load pricing from a TOML string into a flat HashMap keyed by model name.
///
/// The TOML has provider sections (like default_tiers.toml) but the model names
/// are globally unique, so we flatten into a single map.
pub fn load_pricing_toml(toml_str: &str) -> Result<HashMap<String, ModelPricing>> {
    let table: toml::Table = toml_str.parse()?;
    let now = Utc::now().to_rfc3339();
    let mut pricing = HashMap::new();

    fn insert_models(
        models: &toml::map::Map<String, toml::Value>,
        pricing: &mut HashMap<String, ModelPricing>,
        now: &str,
    ) {
        for (key, val) in models {
            // Try as pricing entry first: "model" = { input = X, output = Y }
            if let Ok(entry) = val.clone().try_into::<PricingEntry>() {
                pricing.insert(
                    key.clone(),
                    ModelPricing {
                        model: key.clone(),
                        input_per_million: entry.input,
                        output_per_million: entry.output,
                        cache_read_per_million: entry.cache_read,
                        cache_write_per_million: entry.cache_write,
                        source: Some("default".into()),
                        updated_at: Some(now.to_string()),
                    },
                );
            } else if let toml::Value::Table(sub) = val {
                // Nested section (meta-provider upstream)
                insert_models(sub, pricing, now);
            }
        }
    }

    for (_provider_key, value) in &table {
        if let toml::Value::Table(models) = value {
            insert_models(models, &mut pricing, &now);
        }
    }

    Ok(pricing)
}

/// Merge additional pricing TOML on top (user overrides).
pub fn merge_pricing_toml(base: &mut HashMap<String, ModelPricing>, toml_str: &str) -> Result<()> {
    let overrides = load_pricing_toml(toml_str)?;
    for (model, mut pricing) in overrides {
        pricing.source = Some("override".into());
        base.insert(model, pricing);
    }
    Ok(())
}

// ─── Default pricing fallback ────────────────────────────────────────────────

fn default_pricing() -> HashMap<String, ModelPricing> {
    let now = Utc::now().to_rfc3339();
    // Minimal fallback in case TOML loading fails. Anthropic 5-min ephemeral
    // pricing: cache read = 0.10× input, cache write = 1.25× input.
    let mut m = HashMap::new();
    for (model, input, output) in [
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("claude-opus-4-6", 5.0, 25.0),
        ("claude-haiku-4-6", 1.0, 5.0),
    ] {
        m.insert(
            model.to_string(),
            ModelPricing {
                model: model.to_string(),
                input_per_million: input,
                output_per_million: output,
                cache_read_per_million: Some(input * 0.10),
                cache_write_per_million: Some(input * 1.25),
                source: Some("default".into()),
                updated_at: Some(now.clone()),
            },
        );
    }
    m
}

// ─── CostTracker ─────────────────────────────────────────────────────────────

impl CostTracker {
    pub fn new() -> Self {
        Self {
            records: RwLock::new(Vec::new()),
            ledger_keys: Mutex::new(HashSet::new()),
            model_pricing: RwLock::new(default_pricing()),
            store: None,
        }
    }

    /// Create a cost tracker backed by a persistent telemetry store.
    pub fn with_store(store: Arc<dyn TelemetryStore>) -> Self {
        Self {
            records: RwLock::new(Vec::new()),
            ledger_keys: Mutex::new(HashSet::new()),
            model_pricing: RwLock::new(default_pricing()),
            store: Some(store),
        }
    }

    /// Borrow the backing telemetry store, if any.
    pub fn telemetry_store(&self) -> Option<&Arc<dyn TelemetryStore>> {
        self.store.as_ref()
    }

    /// Fire-and-forget append of a lesson-pipeline event. No-op without a
    /// backing store; never surfaces failure to the caller (purely a
    /// write-side analytics signal).
    pub fn record_lesson_event(&self, event: &crate::telemetry::LessonEvent) {
        if let Some(store) = &self.store {
            if let Err(e) = store.record_lesson_event(event) {
                tracing::warn!(error = %e, "Failed to record lesson event");
            }
        }
    }

    /// Load pricing from embedded TOML, optionally merging user overrides.
    pub fn load_pricing_toml(&self, default_toml: &str, user_toml: Option<&str>) {
        match load_pricing_toml(default_toml) {
            Ok(mut pricing) => {
                if let Some(user) = user_toml {
                    if let Err(e) = merge_pricing_toml(&mut pricing, user) {
                        tracing::warn!(error = %e, "Failed to parse user pricing.toml, using defaults");
                    }
                }
                *self.model_pricing.write() = pricing;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse default pricing TOML, using fallback");
            }
        }
    }

    /// Merge an additional pricing TOML on top of the current pricing map.
    ///
    /// Used to layer the vision-pricing defaults (and future extensions)
    /// without overwriting user overrides. Entries with the same model name
    /// replace the existing entry; new model names are added.
    pub fn merge_pricing_toml_str(&self, toml_str: &str) -> Result<()> {
        let mut pricing = self.model_pricing.write();
        merge_pricing_toml(&mut pricing, toml_str)
    }

    /// Record a usage event, calculating cost from pricing. A served call is
    /// billed to the subscription and keyed under its route. An entry whose
    /// ledger key is already recorded is dropped.
    pub fn record(&self, entry: CostEntry<'_>) {
        let model = entry.served.as_ref().map_or_else(
            || entry.model.to_string(),
            |served| served.ledger_model(entry.model),
        );
        let tokens = entry.tokens;
        let cost = self.calculate_cost(
            &model,
            tokens.input,
            tokens.output,
            tokens.cache_creation,
            tokens.cache_read,
        );
        let record = CostRecord {
            agent_id: entry.agent_id.to_string(),
            model,
            input_tokens: tokens.input,
            output_tokens: tokens.output,
            cache_creation_input_tokens: tokens.cache_creation,
            cache_read_input_tokens: tokens.cache_read,
            cost_usd: cost,
            team_id: entry.team_id.map(String::from),
            task_id: entry.task_id.map(String::from),
            task_type: entry.task_type.map(String::from),
            ticket_id: entry.ticket_id.map(String::from),
            identity_id: entry.identity_id.map(String::from),
            description: entry.description.map(String::from),
            origin: Some(entry.origin),
            billing: Billing::of(entry.served.as_ref()),
            payer: entry.served.as_ref().map(|served| served.payer.clone()),
            credential_tier: entry.served.as_ref().map(|served| served.tier),
            ledger_key: entry.ledger_key.map(String::from),
        };
        let persisted = self.store.as_ref().is_none_or(|store| {
            store.record_cost(&record).unwrap_or_else(|e| {
                tracing::warn!("Failed to persist cost record: {}", e);
                true
            })
        });
        if !persisted {
            return;
        }
        if record
            .ledger_key
            .as_ref()
            .is_some_and(|key| !self.ledger_keys.lock().insert(key.clone()))
        {
            return;
        }
        self.records.write().push(UsageRecord::from(record));
    }

    /// Calculate cost for a given model and token breakdown.
    ///
    /// `input_tokens` should be the fresh (non-cached) input portion only —
    /// cache hits and writes are billed via their own buckets. The Anthropic
    /// API already reports `usage.input_tokens` as the non-cached portion;
    /// other providers normalise to the same shape via their adapters.
    pub fn calculate_cost(
        &self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
    ) -> f64 {
        let pricing = self.model_pricing.read();
        if let Some(p) = pricing.get(model) {
            let cache_read_rate = p.cache_read_per_million.unwrap_or(p.input_per_million);
            let cache_write_rate = p.cache_write_per_million.unwrap_or(p.input_per_million);
            (input_tokens as f64 * p.input_per_million / 1_000_000.0)
                + (output_tokens as f64 * p.output_per_million / 1_000_000.0)
                + (cache_creation_input_tokens as f64 * cache_write_rate / 1_000_000.0)
                + (cache_read_input_tokens as f64 * cache_read_rate / 1_000_000.0)
        } else {
            // Default estimate for unknown models ($3/$15 per M — Sonnet-tier).
            // Cache rates default to 0.10× / 1.25× of the input rate.
            let input_rate = 3.0;
            (input_tokens as f64 * input_rate / 1_000_000.0)
                + (output_tokens as f64 * 15.0 / 1_000_000.0)
                + (cache_creation_input_tokens as f64 * input_rate * 1.25 / 1_000_000.0)
                + (cache_read_input_tokens as f64 * input_rate * 0.10 / 1_000_000.0)
        }
    }

    /// Whether the pricing map has an entry for `model`. A `false` means
    /// [`Self::calculate_cost`] would fall to the Sonnet-tier default, so
    /// callers can surface mispricing instead of silently estimating.
    pub fn is_priced(&self, model: &str) -> bool {
        self.model_pricing.read().contains_key(model)
    }

    /// Set pricing for a model.
    pub fn set_pricing(&self, pricing: ModelPricing) {
        self.model_pricing
            .write()
            .insert(pricing.model.clone(), pricing);
    }

    /// Get a cost summary for a time range.
    pub fn summary(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> CostSummary {
        let records = self.records.read();
        let mut summary = CostSummary::default();

        for record in records
            .iter()
            .filter(|r| r.timestamp >= from && r.timestamp <= to)
        {
            summary.total_cost += record.cost_usd;
            summary.total_input_tokens += record.input_tokens;
            summary.total_output_tokens += record.output_tokens;
            summary.total_cache_creation_input_tokens += record.cache_creation_input_tokens;
            summary.total_cache_read_input_tokens += record.cache_read_input_tokens;
            *summary.by_agent.entry(record.agent_id.clone()).or_default() += record.cost_usd;
            *summary.by_model.entry(record.model.clone()).or_default() += record.cost_usd;
            summary.record_count += 1;
        }

        summary
    }

    /// Check if a budget limit has been exceeded. Budgets count metered
    /// spend only.
    pub fn check_budget(
        &self,
        budget: &Budget,
        agent_id: Option<&str>,
        team_id: Option<&str>,
        session_id: Option<&str>,
    ) -> BudgetStatus {
        let now = Utc::now();
        let today_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap_or(now.naive_utc());
        let today_start = DateTime::<Utc>::from_naive_utc_and_offset(today_start, Utc);

        let recorded = self.records.read();
        let records: Vec<&UsageRecord> = recorded
            .iter()
            .filter(|r| r.billing == Billing::Metered)
            .collect();

        let daily_cost: f64 = records
            .iter()
            .filter(|r| r.timestamp >= today_start)
            .map(|r| r.cost_usd)
            .sum();

        let agent_cost: f64 = if let Some(aid) = agent_id {
            records
                .iter()
                .filter(|r| r.timestamp >= today_start && r.agent_id == aid)
                .map(|r| r.cost_usd)
                .sum()
        } else {
            0.0
        };

        let team_cost: f64 = if let Some(tid) = team_id {
            records
                .iter()
                .filter(|r| r.timestamp >= today_start && r.team_id.as_deref() == Some(tid))
                .map(|r| r.cost_usd)
                .sum()
        } else {
            0.0
        };

        let session_cost: f64 = if let Some(sid) = session_id {
            records
                .iter()
                .filter(|r| r.session_id.as_deref() == Some(sid))
                .map(|r| r.cost_usd)
                .sum()
        } else {
            0.0
        };

        BudgetStatus {
            daily_cost,
            daily_limit: budget.daily_limit,
            daily_exceeded: budget.daily_limit > 0.0 && daily_cost >= budget.daily_limit,
            agent_cost,
            agent_limit: budget.per_agent_limit,
            agent_exceeded: budget.per_agent_limit > 0.0 && agent_cost >= budget.per_agent_limit,
            team_cost,
            team_limit: budget.per_team_limit,
            team_exceeded: budget.per_team_limit > 0.0 && team_cost >= budget.per_team_limit,
            session_cost,
            session_limit: budget.per_session_limit,
            session_exceeded: budget.per_session_limit > 0.0
                && session_cost >= budget.per_session_limit,
        }
    }

    /// Load persisted pricing from SQLite, overriding defaults.
    pub fn load_pricing_from_store(&self) {
        if let Some(ref store) = self.store {
            if let Ok(entries) = store.list_model_pricing() {
                let mut pricing = self.model_pricing.write();
                for entry in entries {
                    pricing.insert(
                        entry.model.clone(),
                        ModelPricing {
                            model: entry.model,
                            input_per_million: entry.input_per_million,
                            output_per_million: entry.output_per_million,
                            // Persisted pricing predates cache rates; let
                            // calculate_cost fall back to input_per_million.
                            cache_read_per_million: None,
                            cache_write_per_million: None,
                            source: Some(entry.source),
                            updated_at: Some(entry.updated_at),
                        },
                    );
                }
            }
        }
    }

    /// Return all current pricing entries.
    pub fn get_all_pricing(&self) -> Vec<ModelPricing> {
        let pricing = self.model_pricing.read();
        pricing.values().cloned().collect()
    }

    /// Get total records count.
    pub fn record_count(&self) -> usize {
        self.records.read().len()
    }
}

impl From<CostRecord> for UsageRecord {
    fn from(record: CostRecord) -> Self {
        Self {
            agent_id: record.agent_id,
            team_id: record.team_id,
            model: record.model,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cache_creation_input_tokens: record.cache_creation_input_tokens,
            cache_read_input_tokens: record.cache_read_input_tokens,
            cost_usd: record.cost_usd,
            timestamp: Utc::now(),
            task_id: record.task_id,
            task_type: record.task_type,
            session_id: None,
            ticket_id: record.ticket_id,
            identity_id: record.identity_id,
            description: record.description,
            origin: record.origin,
            billing: record.billing,
            payer: record.payer,
            credential_tier: record.credential_tier,
            ledger_key: record.ledger_key,
        }
    }
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Budget check result.
#[derive(Debug, Clone)]
pub struct BudgetStatus {
    pub daily_cost: f64,
    pub daily_limit: f64,
    pub daily_exceeded: bool,
    pub agent_cost: f64,
    pub agent_limit: f64,
    pub agent_exceeded: bool,
    pub team_cost: f64,
    pub team_limit: f64,
    pub team_exceeded: bool,
    pub session_cost: f64,
    pub session_limit: f64,
    pub session_exceeded: bool,
}

impl BudgetStatus {
    pub fn any_exceeded(&self) -> bool {
        self.daily_exceeded || self.agent_exceeded || self.team_exceeded || self.session_exceeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn<'a>(agent_id: &'a str, model: &'a str, input: u64, output: u64) -> CostEntry<'a> {
        CostEntry::new(
            agent_id,
            model,
            TokenCounts {
                input,
                output,
                ..TokenCounts::default()
            },
            Origin::Interactive,
        )
    }

    #[test]
    fn load_pricing_toml_parses() {
        let toml = r#"
[anthropic]
"claude-opus-4-6" = { input = 5.00, output = 25.00 }
"claude-sonnet-4-6" = { input = 3.00, output = 15.00 }

[openrouter.deepseek]
"deepseek/deepseek-v3.2" = { input = 0.28, output = 0.42 }
"#;
        let pricing = load_pricing_toml(toml).unwrap();
        assert_eq!(pricing.len(), 3);
        assert!((pricing["claude-opus-4-6"].input_per_million - 5.0).abs() < 0.01);
        assert!((pricing["deepseek/deepseek-v3.2"].output_per_million - 0.42).abs() < 0.01);
    }

    #[test]
    fn merge_pricing_toml_str_layers_on_existing_map() {
        // Simulates the startup flow: defaults → user override → vision merge.
        let tracker = CostTracker::new();
        tracker.load_pricing_toml(
            r#"
[anthropic]
"claude-haiku-4-5" = { input = 1.00, output = 5.00 }
"#,
            None,
        );

        tracker
            .merge_pricing_toml_str(
                r#"
[openrouter.glm-v]
"z-ai/glm-4.5v" = { input = 0.60, output = 1.80 }
"#,
            )
            .unwrap();

        // Vision model now priced — 0.60 * 1M input + 1.80 * 1M output = 2.40
        let cost = tracker.calculate_cost("z-ai/glm-4.5v", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 2.40).abs() < 0.001, "got cost {cost}");
        // Pre-existing entries preserved through the merge.
        let haiku = tracker.calculate_cost("claude-haiku-4-5", 1_000_000, 0, 0, 0);
        assert!((haiku - 1.00).abs() < 0.001);
    }

    #[test]
    fn merge_pricing_overrides() {
        let mut base = load_pricing_toml(
            r#"
[anthropic]
"claude-opus-4-6" = { input = 5.00, output = 25.00 }
"#,
        )
        .unwrap();

        merge_pricing_toml(
            &mut base,
            r#"
[anthropic]
"claude-opus-4-6" = { input = 10.00, output = 50.00 }
"#,
        )
        .unwrap();

        assert!((base["claude-opus-4-6"].input_per_million - 10.0).abs() < 0.01);
        assert_eq!(base["claude-opus-4-6"].source.as_deref(), Some("override"));
    }

    #[test]
    fn record_and_summarize() {
        let tracker = CostTracker::new();
        tracker.record(turn("dev-1", "claude-sonnet-4-6", 1000, 500));
        tracker.record(turn("dev-2", "claude-sonnet-4-6", 2000, 1000));

        let from = Utc::now() - chrono::Duration::hours(1);
        let to = Utc::now() + chrono::Duration::hours(1);
        let summary = tracker.summary(from, to);

        assert_eq!(summary.record_count, 2);
        assert!(summary.total_cost > 0.0);
        assert_eq!(summary.by_agent.len(), 2);
        assert_eq!(summary.by_model.len(), 1);
    }

    fn stored_rows(store: &crate::persistence::SqliteStore) -> Vec<[Option<String>; 5]> {
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT model, origin, billing, payer, credential_tier FROM cost_records ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    fn served(route: crate::subscription::SubscriptionRoute) -> Served {
        Served {
            route,
            tier: CredentialTier::AgentShared,
            payer: "agent:allan".into(),
        }
    }

    #[test]
    fn cost_row_records_origin_billing_payer_tier() {
        let store = Arc::new(crate::persistence::SqliteStore::in_memory().unwrap());
        let tracker = CostTracker::with_store(store.clone());
        tracker.record(CostEntry {
            origin: Origin::Heartbeat,
            ..turn("allan", "gemini-3.6-flash", 10, 5)
        });
        tracker.record(CostEntry {
            origin: Origin::Scheduled,
            served: Some(served(crate::subscription::SubscriptionRoute::Codex)),
            ..turn("allan", "gpt-6.1-sol", 10, 5)
        });

        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            stored_rows(&store),
            [
                [
                    some("gemini-3.6-flash"),
                    some("heartbeat"),
                    some("metered"),
                    None,
                    None
                ],
                [
                    some("codex:gpt-6.1-sol"),
                    some("scheduled"),
                    some("subscription"),
                    some("agent:allan"),
                    some("agent_shared")
                ],
            ]
        );
        let in_memory = tracker.records.read();
        assert_eq!(in_memory[1].billing, Billing::Subscription);
        assert_eq!(in_memory[1].origin, Some(Origin::Scheduled));
        assert_eq!(
            in_memory[1].credential_tier,
            Some(CredentialTier::AgentShared)
        );
    }

    #[test]
    fn subscription_row_uses_served_ledger_key() {
        let store = Arc::new(crate::persistence::SqliteStore::in_memory().unwrap());
        let tracker = CostTracker::with_store(store.clone());
        tracker.set_pricing(ModelPricing {
            model: "chatgpt:gpt-6.1-sol".to_string(),
            input_per_million: 5.0,
            output_per_million: 30.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("default".into()),
            updated_at: None,
        });
        tracker.record(CostEntry {
            served: Some(served(crate::subscription::SubscriptionRoute::ChatGpt)),
            ..turn("allan", "gpt-6.1-sol", 1_000_000, 1_000_000)
        });

        assert_eq!(
            stored_rows(&store)[0][0].as_deref(),
            Some("chatgpt:gpt-6.1-sol")
        );
        let record = &tracker.records.read()[0];
        assert_eq!(record.model, "chatgpt:gpt-6.1-sol");
        assert!((record.cost_usd - 35.0).abs() < 1e-9, "{}", record.cost_usd);
    }

    #[test]
    fn calculate_cost_known_model() {
        let tracker = CostTracker::new();
        let cost = tracker.calculate_cost("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 18.0).abs() < 0.01);
    }

    #[test]
    fn calculate_cost_unknown_model() {
        let tracker = CostTracker::new();
        let cost = tracker.calculate_cost("unknown-model", 1_000_000, 1_000_000, 0, 0);
        assert!(cost > 0.0);
    }

    #[test]
    fn is_priced_reflects_the_pricing_map() {
        let tracker = CostTracker::new();
        // Present in default_pricing.
        assert!(tracker.is_priced("claude-sonnet-4-6"));
        // Absent — calculate_cost would fall to the Sonnet-tier default.
        assert!(!tracker.is_priced("eu.anthropic.claude-sonnet-4-6"));
        assert!(!tracker.is_priced("unknown-model"));
        // A freshly-set model is then priced.
        tracker.set_pricing(ModelPricing {
            model: "newly-priced".to_string(),
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("manual".into()),
            updated_at: Some(Utc::now().to_rfc3339()),
        });
        assert!(tracker.is_priced("newly-priced"));
    }

    /// Cache reads should be 10× cheaper than fresh input on a default
    /// Anthropic-tier model; cache writes 1.25× more expensive.
    #[test]
    fn calculate_cost_cache_aware() {
        let tracker = CostTracker::new();
        // claude-sonnet-4-6: input $3/M, output $15/M
        // → cache_read = $0.30/M, cache_write = $3.75/M
        // 1M fresh + 1M cache_read + 1M cache_write + 1M output
        // = 3.00 + 0.30 + 3.75 + 15.00 = 22.05
        let cost = tracker.calculate_cost(
            "claude-sonnet-4-6",
            1_000_000,
            1_000_000,
            1_000_000,
            1_000_000,
        );
        assert!((cost - 22.05).abs() < 0.01, "expected 22.05, got {cost}");

        // Without cache rates, cache tokens fall back to input rate.
        tracker.set_pricing(ModelPricing {
            model: "no-cache-rates".to_string(),
            input_per_million: 4.0,
            output_per_million: 20.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("manual".into()),
            updated_at: Some(Utc::now().to_rfc3339()),
        });
        // 1M each → input $4 + output $20 + cache_write $4 + cache_read $4 = $32
        let cost =
            tracker.calculate_cost("no-cache-rates", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((cost - 32.0).abs() < 0.01, "expected 32.0, got {cost}");
    }

    #[test]
    fn budget_check_not_exceeded() {
        let tracker = CostTracker::new();
        tracker.record(turn("dev-1", "claude-sonnet-4-6", 1000, 500));

        let budget = Budget {
            daily_limit: 100.0,
            per_agent_limit: 50.0,
            ..Default::default()
        };

        let status = tracker.check_budget(&budget, Some("dev-1"), None, None);
        assert!(!status.any_exceeded());
    }

    #[test]
    fn budget_check_exceeded() {
        let tracker = CostTracker::new();
        for _ in 0..1000 {
            tracker.record(turn("dev-1", "claude-opus-4-6", 100_000, 50_000));
        }

        let budget = Budget {
            daily_limit: 10.0,
            per_agent_limit: 5.0,
            ..Default::default()
        };

        let status = tracker.check_budget(&budget, Some("dev-1"), None, None);
        assert!(status.daily_exceeded);
        assert!(status.agent_exceeded);
    }

    #[test]
    fn budget_check_ignores_subscription_spend() {
        let tracker = CostTracker::new();
        for _ in 0..1000 {
            tracker.record(CostEntry {
                served: Some(served(crate::subscription::SubscriptionRoute::ChatGpt)),
                ..turn("dev-1", "claude-opus-4-6", 100_000, 50_000)
            });
        }
        let budget = Budget {
            daily_limit: 10.0,
            per_agent_limit: 5.0,
            ..Default::default()
        };
        assert!(!tracker
            .check_budget(&budget, Some("dev-1"), None, None)
            .any_exceeded());
    }

    #[test]
    fn custom_pricing() {
        let tracker = CostTracker::new();
        tracker.set_pricing(ModelPricing {
            model: "my-model".to_string(),
            input_per_million: 1.0,
            output_per_million: 2.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("manual".into()),
            updated_at: Some(Utc::now().to_rfc3339()),
        });

        let cost = tracker.calculate_cost("my-model", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 3.0).abs() < 0.01);
    }

    #[test]
    fn load_pricing_from_store_overrides_defaults() {
        use crate::persistence::{SqliteStore, StoredModelPricing};

        let store = Arc::new(SqliteStore::in_memory().unwrap());
        let now = Utc::now().to_rfc3339();

        store
            .upsert_model_pricing(&StoredModelPricing {
                model: "claude-sonnet-4-6".to_string(),
                provider: "anthropic".to_string(),
                input_per_million: 5.0,
                output_per_million: 25.0,
                source: "manual".to_string(),
                fetched_at: now.clone(),
                updated_at: now,
            })
            .unwrap();

        let tracker = CostTracker::with_store(store);
        let cost_before = tracker.calculate_cost("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0);
        assert!((cost_before - 18.0).abs() < 0.01);

        tracker.load_pricing_from_store();

        let cost_after = tracker.calculate_cost("claude-sonnet-4-6", 1_000_000, 1_000_000, 0, 0);
        assert!((cost_after - 30.0).abs() < 0.01);
    }

    #[test]
    fn get_all_pricing_includes_metadata() {
        let tracker = CostTracker::new();
        tracker.set_pricing(ModelPricing {
            model: "test-model".to_string(),
            input_per_million: 2.0,
            output_per_million: 10.0,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: Some("api_fetch".into()),
            updated_at: Some(Utc::now().to_rfc3339()),
        });

        let all = tracker.get_all_pricing();
        assert!(!all.is_empty());

        let test_entry = all.iter().find(|p| p.model == "test-model");
        assert!(test_entry.is_some());
        assert_eq!(test_entry.unwrap().source.as_deref(), Some("api_fetch"));

        let default_entry = all.iter().find(|p| p.model == "claude-sonnet-4-6");
        assert!(default_entry.is_some());
        assert_eq!(default_entry.unwrap().source.as_deref(), Some("default"));
    }

    #[test]
    fn tracker_loads_toml_pricing() {
        let tracker = CostTracker::new();
        tracker.load_pricing_toml(
            r#"
[gemini]
"gemini-2.0-flash" = { input = 0.10, output = 0.40 }
"#,
            None,
        );

        let cost = tracker.calculate_cost("gemini-2.0-flash", 1_000_000, 1_000_000, 0, 0);
        assert!((cost - 0.50).abs() < 0.01);
    }

    /// Regression: when callers pass *fresh* input + cache reads (the
    /// TokenUsage invariant), cached tokens are billed exactly once at the
    /// discounted rate. If a caller mistakenly folded cache into input, this
    /// test would catch the resulting double-bill.
    #[test]
    fn calculate_cost_does_not_double_bill_cached_tokens() {
        let tracker = CostTracker::new();
        // 800k cache hits + 200k fresh on claude-sonnet-4-6:
        //   fresh:      200_000 * $3.00/M = $0.60
        //   cache_read: 800_000 * $0.30/M = $0.24
        //   output:      50_000 * $15.00/M = $0.75
        // Total: $1.59
        let cost = tracker.calculate_cost("claude-sonnet-4-6", 200_000, 50_000, 0, 800_000);
        assert!((cost - 1.59).abs() < 0.001, "expected $1.59, got ${cost}");
    }

    #[test]
    fn tracker_loads_toml_with_cache_rates() {
        let tracker = CostTracker::new();
        tracker.load_pricing_toml(
            r#"
[anthropic]
"claude-test" = { input = 3.00, output = 15.00, cache_read = 0.30, cache_write = 3.75 }
"#,
            None,
        );

        // 1M fresh = 3.00, 1M cache_read = 0.30, 1M cache_write = 3.75, 1M output = 15.00
        let cost = tracker.calculate_cost("claude-test", 1_000_000, 0, 1_000_000, 1_000_000);
        assert!(
            (cost - (3.00 + 3.75 + 0.30)).abs() < 0.01,
            "expected 7.05, got {cost}"
        );
    }

    fn round(ledger_key: Option<&str>) -> CostEntry<'_> {
        CostEntry {
            ledger_key,
            ..turn("allan", "claude-code:sonnet", 1_000_000, 0)
        }
    }

    #[test]
    fn in_memory_records_skip_duplicate_ledger_key() {
        let tracker = CostTracker::new();
        tracker.record(round(Some("r1.0.0")));
        tracker.record(round(Some("r1.0.0")));
        tracker.record(round(Some("r1.0.1")));
        let keys: Vec<Option<String>> = tracker
            .records
            .read()
            .iter()
            .map(|r| r.ledger_key.clone())
            .collect();
        assert_eq!(
            keys,
            [Some("r1.0.0".to_string()), Some("r1.0.1".to_string())]
        );
    }

    #[test]
    fn unkeyed_rounds_still_record_every_row() {
        let store = Arc::new(crate::persistence::SqliteStore::in_memory().unwrap());
        let tracker = CostTracker::with_store(Arc::clone(&store) as Arc<dyn TelemetryStore>);
        tracker.record(round(None));
        tracker.record(round(None));
        tracker.record(round(Some("r1.0.0")));
        tracker.record(round(Some("r1.0.0")));

        let per_row = tracker.calculate_cost("claude-code:sonnet", 1_000_000, 0, 0, 0);
        let persisted = store
            .total_cost(Utc::now() - chrono::Duration::hours(1), Utc::now())
            .unwrap();
        assert!(
            (persisted - 3.0 * per_row).abs() < 1e-9,
            "two unkeyed rows and one keyed row: {persisted}"
        );
        assert_eq!(tracker.records.read().len(), 3);
        assert!(
            !store
                .record_cost(&CostRecord {
                    ledger_key: Some("r1.0.0".into()),
                    ..Default::default()
                })
                .unwrap(),
            "the store reports a repeated key as not inserted"
        );
    }
}
