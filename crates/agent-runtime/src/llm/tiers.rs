//! Model tier equivalence system — maps models across providers to capability tiers.
//!
//! Tier definitions are loaded from `default_tiers.toml` (embedded at compile time)
//! and can be extended with a workspace `tiers.toml` and `[llm.equivalences]` overrides.
//!
//! Each provider (or meta-provider upstream like `openrouter.deepseek`) maps tier names
//! (`cheap`, `standard`, `powerful`) to concrete model IDs.

use std::collections::HashMap;

use eyre::{bail, Result};

use super::provider::{ProviderKind, ProviderParseError};

/// Live, shared `[llm.equivalences]` overrides used to build a [`TierRegistry`].
/// Held behind an `Arc<RwLock>` — like the hub's `tier_key` — so a config
/// hot-reload updates every reader at once, including the wire-level pool
/// resolver. The inner `Arc` keeps reads cheap (clone the handle, not the map).
pub type EquivalencesCell =
    std::sync::Arc<parking_lot::RwLock<Option<std::sync::Arc<HashMap<String, Vec<String>>>>>>;

/// Capability tier for model classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelTier {
    /// Budget models for simple, repetitive tasks (e.g. Haiku, Flash Lite).
    Cheap,
    /// Mid-range models balancing cost and capability (e.g. Sonnet, Flash).
    Standard,
    /// Frontier models for complex reasoning (e.g. Opus, Grok-4, Gemini 2.5 Pro).
    Powerful,
}

impl ModelTier {
    pub const ALL: [Self; 3] = [Self::Powerful, Self::Standard, Self::Cheap];

    /// Parse a tier name (case-insensitive).
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "cheap" => Some(Self::Cheap),
            "standard" => Some(Self::Standard),
            "powerful" => Some(Self::Powerful),
            _ => None,
        }
    }

    /// Tier name as a string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Standard => "standard",
            Self::Powerful => "powerful",
        }
    }
}

/// A provider identifier derived from a model string or chain entry.
///
/// Used by the failover system to parse "provider:model" chain entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    Grok,
    Gemini,
    Ollama,
    OpenRouter,
    Cerebras,
    Zai,
    Bedrock,
    ChatGpt,
    ClaudeCode,
    Codex,
    Unknown,
}

impl Provider {
    /// Infer the provider from a model name or "provider:model" chain entry.
    pub fn from_model_str(s: &str) -> (Self, &str) {
        if is_bedrock_model_id(s) {
            return (Provider::Bedrock, s);
        }
        if let (Some(prefix), model) = split_routing_prefix(s) {
            let provider = Self::subscription(prefix).unwrap_or_else(|| match prefix {
                "grok" => Provider::Grok,
                "gemini" => Provider::Gemini,
                "ollama" => Provider::Ollama,
                "cerebras" => Provider::Cerebras,
                "zai" => Provider::Zai,
                "bedrock" => Provider::Bedrock,
                _ if prefix.starts_with("openrouter") => Provider::OpenRouter,
                _ => Provider::Unknown,
            });
            (provider, model)
        } else {
            let provider = if s.contains("claude")
                || s.contains("haiku")
                || s.contains("sonnet")
                || s.contains("opus")
            {
                Provider::Anthropic
            } else if s.starts_with("grok") {
                Provider::Grok
            } else if s.starts_with("gemini") {
                Provider::Gemini
            } else if s.starts_with("glm-") {
                Provider::Zai
            } else {
                Provider::Unknown
            };
            (provider, s)
        }
    }

    /// Parse a provider name (used for provider-only failover entries like "gemini").
    pub fn from_name(name: &str) -> Option<Self> {
        Self::subscription(name).or_else(|| match name {
            "anthropic" => Some(Provider::Anthropic),
            "grok" => Some(Provider::Grok),
            "gemini" => Some(Provider::Gemini),
            "ollama" => Some(Provider::Ollama),
            "cerebras" => Some(Provider::Cerebras),
            "zai" | "z-ai" | "zhipu" | "bigmodel" => Some(Provider::Zai),
            "bedrock" | "aws" | "aws-bedrock" => Some(Provider::Bedrock),
            _ => {
                if name.starts_with("openrouter") {
                    Some(Provider::OpenRouter)
                } else if name.starts_with("cerebras") {
                    Some(Provider::Cerebras)
                } else if name.starts_with("bedrock") {
                    Some(Provider::Bedrock)
                } else {
                    None
                }
            }
        })
    }

    /// The subscription provider `name` names, as [`ProviderKind::parse`]
    /// reads it.
    fn subscription(name: &str) -> Option<Self> {
        match ProviderKind::from_str_loose(name)? {
            ProviderKind::ChatGpt => Some(Provider::ChatGpt),
            ProviderKind::ClaudeCode => Some(Provider::ClaudeCode),
            ProviderKind::Codex => Some(Provider::Codex),
            _ => None,
        }
    }
}

/// Split a `"provider:model"` routing spec at its provider prefix.
///
/// Splits only when the prefix is a recognized provider routing key —
/// Bedrock-style model ids (whose `-v1:0` version suffix contains a colon)
/// and unrecognized prefixes pass through whole, so a legitimate model id is
/// never mangled into a bogus (provider, model) pair.
pub fn split_routing_prefix(s: &str) -> (Option<&str>, &str) {
    if is_bedrock_model_id(s) {
        return (None, s);
    }
    match s.split_once(':') {
        Some((prefix, rest))
            if Provider::from_name(prefix).is_some()
                || ProviderKind::from_str_loose(prefix).is_some() =>
        {
            (Some(prefix), rest)
        }
        _ => (None, s),
    }
}

pub(super) fn is_bedrock_model_id(s: &str) -> bool {
    let stripped = ["us.", "eu.", "au.", "jp.", "apac.", "global."]
        .iter()
        .find_map(|p| s.strip_prefix(p))
        .unwrap_or(s);
    matches!(
        stripped.split('.').next(),
        Some(
            "anthropic"
                | "zai"
                | "deepseek"
                | "qwen"
                | "moonshot"
                | "moonshotai"
                | "minimax"
                | "meta"
                | "mistral"
        )
    ) && stripped.contains('.')
}

/// Registry mapping tier names to concrete model IDs, keyed by provider.
///
/// Provider keys use dots for meta-provider upstreams:
/// `"anthropic"`, `"openrouter.deepseek"`, `"gemini"`, etc.
#[derive(Debug, Clone)]
pub struct TierRegistry {
    providers: HashMap<String, HashMap<String, String>>,
}

const DEFAULT_TIERS_TOML: &str = include_str!("default_tiers.toml");

impl TierRegistry {
    /// Load the registry from the embedded defaults.
    pub fn from_defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_TIERS_TOML)
    }

    /// Parse a tiers.toml string into a registry.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        Ok(Self {
            providers: parse_tier_table(toml_str)?,
        })
    }

    /// Merge another tiers.toml on top (user workspace overrides).
    pub fn merge_toml(mut self, toml_str: &str) -> Result<Self> {
        let other = Self::from_toml(toml_str)?;
        for (key, tiers) in other.providers {
            let entry = self.providers.entry(key).or_default();
            for (tier_name, model_id) in tiers {
                entry.insert(tier_name, model_id);
            }
        }
        Ok(self)
    }

    /// Merge `[llm.equivalences]` overrides on top.
    ///
    /// Entries are model strings like `"grok:grok-4"` or `"claude-opus-4-6"`.
    /// The provider prefix determines which section receives the override.
    pub fn with_equivalences(mut self, equivalences: &HashMap<String, Vec<String>>) -> Self {
        for (tier_name, models) in equivalences {
            if ModelTier::from_name(tier_name).is_none() {
                continue;
            }
            for chain_str in models {
                let (provider, model_name) = Provider::from_model_str(chain_str);
                let tier_key = match provider {
                    Provider::Anthropic => "anthropic",
                    Provider::Grok => "grok",
                    Provider::Gemini => "gemini",
                    Provider::Ollama => "ollama",
                    Provider::Zai => "zai",
                    Provider::ChatGpt => ProviderKind::ChatGpt.canonical_name(),
                    Provider::ClaudeCode => ProviderKind::ClaudeCode.canonical_name(),
                    Provider::Codex => ProviderKind::Codex.canonical_name(),
                    Provider::OpenRouter
                    | Provider::Cerebras
                    | Provider::Bedrock
                    | Provider::Unknown => continue,
                };
                self.providers
                    .entry(tier_key.to_string())
                    .or_default()
                    .insert(tier_name.clone(), model_name.to_string());
            }
        }
        self
    }

    /// Create a registry with built-in defaults, optionally merged with equivalence overrides.
    pub fn new(overrides: Option<&HashMap<String, Vec<String>>>) -> Self {
        let mut reg = Self::from_defaults().unwrap_or_else(|_| Self {
            providers: HashMap::new(),
        });
        if let Some(eq) = overrides {
            reg = reg.with_equivalences(eq);
        }
        reg
    }

    /// Build from a live [`EquivalencesCell`]. The single place that turns a
    /// cell into a registry, so cell holders don't repeat the read-clone-deref.
    pub fn from_cell(cell: &EquivalencesCell) -> Self {
        Self::new(cell.read().clone().as_deref())
    }

    /// Iterator over the registry's tier-key namespace, e.g. `"anthropic"`,
    /// `"bedrock.anthropic"`, `"openrouter.deepseek"`. Used together with
    /// [`parse_provider_tier_key`] to derive the `system.set_provider`
    /// tool-schema enum from a single source of truth.
    pub fn tier_keys(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    /// Classify a model into a tier. Falls back to heuristics for unknown models.
    ///
    /// When a model appears in multiple tiers (e.g. grok-3 as both standard and cheap),
    /// returns the highest tier.
    pub fn classify(&self, model: &str) -> ModelTier {
        let (_, model_name) = Provider::from_model_str(model);

        let mut best: Option<ModelTier> = None;
        for tiers in self.providers.values() {
            for (tier_name, model_id) in tiers {
                if model_id == model_name || model_id == model {
                    if let Some(tier) = ModelTier::from_name(tier_name) {
                        best = Some(match best {
                            None => tier,
                            Some(prev) => match (prev, tier) {
                                (ModelTier::Powerful, _) | (_, ModelTier::Powerful) => {
                                    ModelTier::Powerful
                                }
                                (ModelTier::Standard, _) | (_, ModelTier::Standard) => {
                                    ModelTier::Standard
                                }
                                _ => ModelTier::Cheap,
                            },
                        });
                    }
                }
            }
        }
        if let Some(tier) = best {
            return tier;
        }

        // Heuristic fallback based on common naming patterns
        let lower = model_name.to_lowercase();
        if lower.contains("haiku") || lower.contains("flash-lite") || lower.contains("mini") {
            ModelTier::Cheap
        } else if lower.contains("opus") || lower.contains("pro") || lower.contains("grok-4") {
            ModelTier::Powerful
        } else {
            ModelTier::Standard
        }
    }

    /// Get the model ID for a given tier and provider tier key.
    pub fn resolve_for_tier(&self, tier: ModelTier, tier_key: &str) -> Option<String> {
        self.providers
            .get(tier_key)
            .and_then(|tiers| tiers.get(tier.as_str()))
            .cloned()
    }

    /// Resolve the equivalent model for a target provider at the same tier.
    pub fn resolve_equivalent(&self, model: &str, target_tier_key: &str) -> Option<String> {
        let tier = self.classify(model);
        self.resolve_for_tier(tier, target_tier_key)
    }

    /// Resolve a tier name to a concrete model, or pass through a concrete model as-is.
    pub fn resolve_model(&self, model_or_tier: &str, tier_key: &str) -> String {
        if let Some(tier) = ModelTier::from_name(model_or_tier) {
            self.resolve_for_tier(tier, tier_key)
                .unwrap_or_else(|| model_or_tier.to_string())
        } else {
            model_or_tier.to_string()
        }
    }

    /// Resolve a model spec that may include provider routing.
    ///
    /// Supports these formats:
    /// - `"standard"` → uses default_tier_key, returns resolved model
    /// - `"gemini:cheap"` → routes to gemini, returns `"gemini:gemini-3.1-flash-lite-preview"`
    /// - `"anthropic:powerful"` → routes to anthropic, returns `"claude-opus-4-6"`
    /// - `"openrouter:deepseek:standard"` → routes to openrouter, returns `"openrouter:deepseek/deepseek-v3.2"`
    /// - `"claude-sonnet-4-6"` → concrete model, passes through
    ///
    /// Returns `(provider_key, resolved_model)` where provider_key is the failover
    /// routing key (e.g. `"gemini"`, `"grok"`) or None for the default provider.
    pub fn resolve_routed_model(
        &self,
        spec: &str,
        default_tier_key: &str,
    ) -> (Option<String>, String) {
        // 1. Plain tier name — use default provider
        if let Some(tier) = ModelTier::from_name(spec) {
            let model = self
                .resolve_for_tier(tier, default_tier_key)
                .unwrap_or_else(|| spec.to_string());
            return (None, model);
        }

        // 2. Try parsing as "provider:tier" or "provider:upstream:tier"
        if spec.contains(':') {
            // Split off the last segment — could be a tier name
            let last_colon = spec.rfind(':').unwrap();
            let (provider_part, maybe_tier) = spec.split_at(last_colon);
            let maybe_tier = &maybe_tier[1..]; // skip the ':'

            if let Some(tier) = ModelTier::from_name(maybe_tier) {
                // The last segment is a tier name — everything before it is the provider
                if let Ok((provider_name, tier_key)) = parse_provider_tier_key(provider_part) {
                    if let Some(model) = self.resolve_for_tier(tier, &tier_key) {
                        let routing_key = if tier_key == default_tier_key {
                            None
                        } else {
                            Some(provider_name)
                        };
                        return (routing_key, model);
                    }
                }
            }

            // Not a provider:tier combo — could be "provider:concrete_model" like "grok:grok-4"
            // Pass through as-is (failover system handles provider:model format)
            return (None, spec.to_string());
        }

        // 3. Concrete model name — pass through
        (None, spec.to_string())
    }

    /// Format a routed model result as a provider-prefixed string for the failover system.
    ///
    /// When `routing_key` is Some, prefixes the model (e.g. `"gemini:gemini-3-flash-preview"`).
    /// When None, returns the model as-is.
    pub fn format_routed(routing_key: Option<&str>, model: String) -> String {
        match routing_key {
            Some(rk) => format!("{rk}:{model}"),
            None => model,
        }
    }

    /// Given a model and a list of healthy provider tier keys (in preference order),
    /// return the best equivalent model. Tries the same tier first, then
    /// falls back to adjacent tiers.
    pub fn resolve_healthy_equivalent(&self, model: &str, healthy_keys: &[&str]) -> Option<String> {
        let tier = self.classify(model);

        // Try same tier first
        for &key in healthy_keys {
            if let Some(m) = self.resolve_for_tier(tier, key) {
                return Some(m);
            }
        }

        // Fall back to adjacent tier (Powerful → Standard, Cheap → Standard)
        let fallback_tier = match tier {
            ModelTier::Powerful => ModelTier::Standard,
            ModelTier::Cheap => ModelTier::Standard,
            ModelTier::Standard => return None,
        };
        for &key in healthy_keys {
            if let Some(m) = self.resolve_for_tier(fallback_tier, key) {
                return Some(m);
            }
        }

        None
    }

    /// Find which tier key a model belongs to by searching all provider sections.
    pub fn tier_key_for_model(&self, model: &str) -> Option<String> {
        let (_, model_name) = Provider::from_model_str(model);
        for (key, tiers) in &self.providers {
            for model_id in tiers.values() {
                if model_id == model_name || model_id == model {
                    return Some(key.clone());
                }
            }
        }
        None
    }
}

/// Extract the provider routing key from a routed model spec, if any.
///
/// Returns the provider name that would need an external provider initialized.
/// Returns None for plain tiers, concrete models, or specs using the default provider.
pub fn extract_routing_provider(spec: &str) -> Option<String> {
    if ModelTier::from_name(spec).is_some() {
        return None;
    }
    if !spec.contains(':') {
        return None;
    }
    let last_colon = spec.rfind(':').unwrap();
    let (provider_part, maybe_tier) = spec.split_at(last_colon);
    let maybe_tier = &maybe_tier[1..];

    if ModelTier::from_name(maybe_tier).is_some() {
        parse_provider_tier_key(provider_part)
            .ok()
            .map(|(name, _)| name)
    } else {
        // Could be "grok:grok-4" style — extract provider prefix
        split_routing_prefix(spec).0.map(str::to_string)
    }
}

/// Parse a tier-style TOML table into a flat `provider_key → tier_name → model_id`
/// map, handling both flat sections (`[anthropic]`) and nested meta-provider
/// sections (`[openrouter.deepseek]`, which TOML parses as a sub-table).
///
/// Shared by [`TierRegistry`] and the vision tier registry — both files use the
/// same TOML schema.
pub(super) fn parse_tier_table(toml_str: &str) -> Result<HashMap<String, HashMap<String, String>>> {
    let table: toml::Table = toml_str.parse()?;
    let mut providers: HashMap<String, HashMap<String, String>> = HashMap::new();

    for (key, value) in &table {
        let toml::Value::Table(tiers) = value else {
            continue;
        };
        let has_sub_tables = tiers.values().any(|v| matches!(v, toml::Value::Table(_)));

        if !has_sub_tables {
            let tier_map = collect_string_entries(tiers);
            if !tier_map.is_empty() {
                providers.insert(key.clone(), tier_map);
            }
        } else {
            for (upstream, sub_val) in tiers {
                if let toml::Value::Table(sub_tiers) = sub_val {
                    let tier_map = collect_string_entries(sub_tiers);
                    if !tier_map.is_empty() {
                        providers.insert(format!("{key}.{upstream}"), tier_map);
                    }
                }
            }
        }
    }
    Ok(providers)
}

fn collect_string_entries(table: &toml::Table) -> HashMap<String, String> {
    table
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

/// The provider kind and tier key a `[llm] provider` spec names. An unknown
/// provider is an error, never a silent fallback to another provider; a
/// known one that names no usable tier key (a bare `openrouter`) is `None`.
pub fn parse_provider_spec(spec: &str) -> Result<Option<(ProviderKind, String)>> {
    let kind = ProviderKind::parse(spec.split(':').next().unwrap_or_default().trim())?;
    Ok(parse_provider_tier_key(spec)
        .ok()
        .map(|(_, tier_key)| (kind, tier_key)))
}

/// Parse a provider string into `(provider_name, tier_key)`.
///
/// `provider_name` feeds `ProviderKind::from_str_loose()`, `tier_key` feeds `TierRegistry`.
/// Bare `"openrouter"` is an error — must specify upstream like `"openrouter:deepseek"`.
/// This function is the validator paired with [`TierRegistry::tier_keys`] to
/// derive the `system.set_provider` tool-schema enum.
pub fn parse_provider_tier_key(provider_str: &str) -> Result<(String, String)> {
    let base = provider_str.split(':').next().unwrap_or_default();
    if let Err(removed @ ProviderParseError::Removed { .. }) = ProviderKind::parse(base) {
        return Err(removed.into());
    }
    let lower = provider_str.to_lowercase();
    if let Some(kind) = ProviderKind::from_str_loose(&lower).filter(|k| k.is_subscription()) {
        let name = kind.canonical_name().to_string();
        return Ok((name.clone(), name));
    }

    if let Some((base, upstream)) = lower.split_once(':') {
        if base == "openrouter" || base == "open-router" || base == "or" {
            if upstream.is_empty() {
                bail!("openrouter requires an upstream provider: openrouter:<upstream> (e.g. openrouter:deepseek, openrouter:anthropic)");
            }
            return Ok(("openrouter".to_string(), format!("openrouter.{}", upstream)));
        }
        if base == "bedrock" || base == "aws" || base == "aws-bedrock" {
            if upstream.is_empty() {
                bail!("bedrock requires a model family: bedrock:<family> (e.g. bedrock:anthropic, bedrock:zai, bedrock:deepseek, bedrock:qwen, bedrock:kimi, bedrock:minimax, bedrock:llama, bedrock:mistral)");
            }
            return Ok(("bedrock".to_string(), format!("bedrock.{}", upstream)));
        }
        return Ok((base.to_string(), format!("{base}.{upstream}")));
    }

    match lower.as_str() {
        "openrouter" | "open-router" | "or" => {
            bail!("openrouter requires an upstream provider: openrouter:<upstream> (e.g. openrouter:deepseek, openrouter:anthropic)");
        }
        "anthropic" | "claude" => Ok(("anthropic".to_string(), "anthropic".to_string())),
        "grok" | "xai" | "x-ai" => Ok(("grok".to_string(), "grok".to_string())),
        "gemini" | "google" | "google-ai" => Ok(("gemini".to_string(), "gemini".to_string())),
        "ollama" | "local" => Ok(("ollama".to_string(), "ollama".to_string())),
        "vllm" | "vllm-server" | "self-hosted" | "runpod" => {
            Ok(("vllm".to_string(), "vllm".to_string()))
        }
        // Transcription-only providers — no chat completions client exists for
        // these in agent-runtime, but they're valid tier keys for [llm.vision]
        // and [llm.transcription] resolution.
        "openai" => Ok(("openai".to_string(), "openai".to_string())),
        "groq" | "groq-ai" => Ok(("groq".to_string(), "groq".to_string())),
        "cerebras" | "cerebras-ai" => {
            bail!("cerebras requires a model family: cerebras:<family> (e.g. cerebras:qwen, cerebras:gpt, cerebras:llama, cerebras:glm, cerebras:mixed)");
        }
        "zai" | "z-ai" | "zhipu" | "bigmodel" => Ok(("zai".to_string(), "zai".to_string())),
        "bedrock" | "aws" | "aws-bedrock" => {
            bail!("bedrock requires a model family: bedrock:<family> (e.g. bedrock:anthropic, bedrock:zai, bedrock:deepseek, bedrock:qwen, bedrock:kimi, bedrock:minimax, bedrock:llama, bedrock:mistral)");
        }
        _ => bail!("unknown provider: {}", provider_str),
    }
}

/// Why a secondary-model spec (`[llm.vision]`, `[llm.transcription]`,
/// `[llm.image]`, `[llm.video]`) names no model to call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelSpecError {
    #[error(
        "'{spec}' names no known provider: write it as provider:model or provider:tier, or set `provider`"
    )]
    NoProvider { spec: String },
    #[error("'{spec}' asks for the {tier} tier of '{tier_key}', which lists no such model")]
    NoTier {
        spec: String,
        tier_key: String,
        tier: &'static str,
    },
    #[error("'{spec}': {reason}")]
    Provider { spec: String, reason: String },
}

/// Every `provider:tier` spec a tier `table` answers.
pub(super) fn tier_specs(
    table: &HashMap<String, HashMap<String, String>>,
) -> impl Iterator<Item = String> + '_ {
    table.iter().flat_map(|(key, tiers)| {
        tiers
            .keys()
            .map(move |tier| format!("{}:{tier}", key.replace('.', ":")))
    })
}

/// How one tier registry reads the provider a spec names.
pub(super) struct SpecRoutes<S, K> {
    /// Splits a spec at the provider prefix it recognises.
    pub split: S,
    /// A provider string as `(provider_name, tier_key)`.
    pub tier_key: K,
}

/// Resolve a secondary-model spec against its tier `table` into
/// `(provider, model)`:
///   - `"cheap"`: that tier of `default_provider`
///   - `"openrouter:anthropic:cheap"`: that tier of the provider it names
///   - `"chatgpt:gpt-6.1-sol"`: the model, on the route `routes.split` recognises
///   - `"qwen2.5vl:7b"`: the model, on `default_provider`
///
/// A spec that names no provider where none is the default, or a tier of a
/// provider that is unknown or lists none, is an error: no provider is ever
/// assumed.
pub(super) fn resolve_model_spec<S, K>(
    table: &HashMap<String, HashMap<String, String>>,
    spec: &str,
    default_provider: Option<&str>,
    routes: SpecRoutes<S, K>,
) -> Result<(String, String), ModelSpecError>
where
    S: for<'a> Fn(&'a str) -> (Option<&'a str>, &'a str),
    K: Fn(&str) -> Result<(String, String)>,
{
    let refused = |reason: eyre::Report| ModelSpecError::Provider {
        spec: spec.to_string(),
        reason: reason.to_string(),
    };
    let default = || {
        default_provider.ok_or_else(|| ModelSpecError::NoProvider {
            spec: spec.to_string(),
        })
    };
    let tier_of = |provider: &str, tier: ModelTier| {
        let (name, tier_key) = (routes.tier_key)(provider).map_err(refused)?;
        table
            .get(&tier_key)
            .and_then(|tiers| tiers.get(tier.as_str()))
            .map(|model| (name, model.clone()))
            .ok_or_else(|| ModelSpecError::NoTier {
                spec: spec.to_string(),
                tier_key,
                tier: tier.as_str(),
            })
    };
    // A default may carry its upstream (`openrouter:google`), which names a
    // tier table, not a second provider.
    let named = |provider: &str| {
        provider
            .split(':')
            .next()
            .unwrap_or(provider)
            .to_lowercase()
    };

    if let Some(tier) = ModelTier::from_name(spec) {
        return tier_of(default()?, tier);
    }
    let routed = (routes.split)(spec);
    if let Some((provider, tier)) = spec
        .rsplit_once(':')
        .and_then(|(provider, tier)| Some((provider, ModelTier::from_name(tier)?)))
    {
        return match (routes.tier_key)(provider) {
            Err(_) if routed.0.is_none() => Err(ModelSpecError::NoProvider {
                spec: spec.to_string(),
            }),
            _ => tier_of(provider, tier),
        };
    }
    match routed {
        (Some(provider), model) => Ok((named(provider), model.to_string())),
        (None, model) => Ok((named(default()?), model.to_string())),
    }
}

/// Map a thinking budget (token count) to a Gemini thinking level.
///
/// Gemini 3.x models accept `thinking_level` instead of a raw token budget.
/// This converts the user's `thinking_budget_tokens` config to the appropriate level.
pub fn budget_to_thinking_level(budget: u32) -> &'static str {
    match budget {
        0 => "minimal",
        1..=2000 => "low",
        2001..=8000 => "medium",
        _ => "high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_reg() -> TierRegistry {
        TierRegistry::from_defaults().unwrap()
    }

    #[test]
    fn test_from_defaults_loads() {
        let reg = default_reg();
        assert!(reg.providers.contains_key("anthropic"));
        assert!(reg.providers.contains_key("openrouter.deepseek"));
        assert!(reg.providers.contains_key("gemini"));
        assert!(reg.providers.contains_key("grok"));
        assert!(reg.providers.contains_key("bedrock.anthropic"));
        assert!(reg.providers.contains_key("bedrock.zai"));
        assert!(reg.providers.contains_key("bedrock.deepseek"));
        assert!(reg.providers.contains_key("bedrock.qwen"));
        assert!(reg.providers.contains_key("bedrock.kimi"));
        assert!(reg.providers.contains_key("bedrock.minimax"));
        assert!(reg.providers.contains_key("bedrock.llama"));
        assert!(reg.providers.contains_key("bedrock.mistral"));
    }

    #[test]
    fn subscription_providers_route_like_other_providers() {
        let reg = default_reg();
        for (name, provider, powerful, cheap) in [
            ("chatgpt", Provider::ChatGpt, "gpt-6.1-sol", "gpt-6-luna"),
            ("claude-code", Provider::ClaudeCode, "opus", "haiku"),
            ("codex", Provider::Codex, "gpt-6.1-sol", "gpt-6-luna"),
        ] {
            assert_eq!(
                parse_provider_tier_key(name).unwrap(),
                (name.to_string(), name.to_string())
            );
            assert_eq!(Provider::from_name(name), Some(provider));
            assert_eq!(
                reg.resolve_routed_model(&format!("{name}:powerful"), "anthropic"),
                (Some(name.to_string()), powerful.to_string())
            );
            assert_eq!(reg.resolve_model("cheap", name), cheap);
            let routed = format!("{name}:{powerful}");
            assert_eq!(split_routing_prefix(&routed), (Some(name), powerful));
            assert_eq!(Provider::from_model_str(&routed), (provider, powerful));
            assert_eq!(extract_routing_provider(&routed).as_deref(), Some(name));
        }
        assert_eq!(
            parse_provider_tier_key("claude_code").unwrap().0,
            "claude-code"
        );
    }

    #[test]
    fn bedrock_bare_provider_errors_with_family_hint() {
        let err = parse_provider_tier_key("bedrock").unwrap_err().to_string();
        assert!(err.contains("bedrock:<family>"));
    }

    #[test]
    fn bedrock_with_family_parses() {
        let (name, key) = parse_provider_tier_key("bedrock:anthropic").unwrap();
        assert_eq!(name, "bedrock");
        assert_eq!(key, "bedrock.anthropic");

        let (name, key) = parse_provider_tier_key("bedrock:zai").unwrap();
        assert_eq!(name, "bedrock");
        assert_eq!(key, "bedrock.zai");
    }

    #[test]
    fn bedrock_resolve_tier_routes_to_model_id() {
        let reg = default_reg();
        assert_eq!(
            reg.resolve_model("powerful", "bedrock.anthropic"),
            "anthropic.claude-opus-5-5"
        );
        assert_eq!(reg.resolve_model("standard", "bedrock.zai"), "zai.glm-4.7");
        assert_eq!(
            reg.resolve_model("powerful", "bedrock.qwen"),
            "qwen.qwen3-coder-480b-a35b-v1:0"
        );
    }

    #[test]
    fn bedrock_model_id_detected_as_bedrock() {
        let (provider, model) = Provider::from_model_str("anthropic.claude-sonnet-4-6");
        assert_eq!(provider, Provider::Bedrock);
        assert_eq!(model, "anthropic.claude-sonnet-4-6");

        let (provider, _) = Provider::from_model_str("us.zai.glm-5");
        assert_eq!(provider, Provider::Bedrock);

        let (provider, _) = Provider::from_model_str("moonshotai.kimi-k2.5");
        assert_eq!(provider, Provider::Bedrock);

        let (provider, _) = Provider::from_model_str("meta.llama4-maverick-17b-instruct-v1:0");
        assert_eq!(provider, Provider::Bedrock);
    }

    #[test]
    fn split_routing_prefix_keeps_bedrock_ids_whole() {
        assert_eq!(
            split_routing_prefix("anthropic.claude-haiku-4-5-20251001-v1:0"),
            (None, "anthropic.claude-haiku-4-5-20251001-v1:0")
        );
        assert_eq!(
            split_routing_prefix("eu.anthropic.claude-haiku-4-5-20251001-v1:0"),
            (None, "eu.anthropic.claude-haiku-4-5-20251001-v1:0")
        );
    }

    #[test]
    fn split_routing_prefix_splits_recognized_providers_only() {
        assert_eq!(
            split_routing_prefix("anthropic:claude-haiku-4-5"),
            (Some("anthropic"), "claude-haiku-4-5")
        );
        assert_eq!(
            split_routing_prefix("grok:grok-4"),
            (Some("grok"), "grok-4")
        );
        assert_eq!(
            split_routing_prefix("openrouter:deepseek:powerful"),
            (Some("openrouter"), "deepseek:powerful")
        );
        assert_eq!(
            split_routing_prefix("vertex:claude-sonnet-5"),
            (Some("vertex"), "claude-sonnet-5")
        );
        assert_eq!(split_routing_prefix("llama3:8b"), (None, "llama3:8b"));
        assert_eq!(
            split_routing_prefix("claude-sonnet-5"),
            (None, "claude-sonnet-5")
        );
    }

    #[test]
    fn bedrock_from_name_aliases() {
        assert_eq!(Provider::from_name("bedrock"), Some(Provider::Bedrock));
        assert_eq!(Provider::from_name("aws"), Some(Provider::Bedrock));
        assert_eq!(Provider::from_name("aws-bedrock"), Some(Provider::Bedrock));
        assert_eq!(
            Provider::from_name("bedrock.anthropic"),
            Some(Provider::Bedrock)
        );
    }

    #[test]
    fn test_resolve_model_tier_names() {
        let reg = default_reg();
        assert_eq!(reg.resolve_model("cheap", "anthropic"), "claude-haiku-5-5");
        assert_eq!(
            reg.resolve_model("standard", "anthropic"),
            "claude-sonnet-5-5"
        );
        assert_eq!(
            reg.resolve_model("powerful", "anthropic"),
            "claude-opus-5-5"
        );
    }

    #[test]
    fn test_resolve_model_openrouter_upstream() {
        let reg = default_reg();
        assert_eq!(
            reg.resolve_model("standard", "openrouter.deepseek"),
            "deepseek/deepseek-v4.1-flash"
        );
        assert_eq!(
            reg.resolve_model("powerful", "openrouter.anthropic"),
            "anthropic/claude-opus-5.5"
        );
        assert_eq!(
            reg.resolve_model("cheap", "openrouter.google"),
            "google/gemini-3.5-flash-lite"
        );
    }

    #[test]
    fn test_resolve_model_concrete_passthrough() {
        let reg = default_reg();
        assert_eq!(
            reg.resolve_model("claude-sonnet-4-6", "grok"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            reg.resolve_model("deepseek/deepseek-v3.2", "openrouter.deepseek"),
            "deepseek/deepseek-v3.2"
        );
    }

    #[test]
    fn test_resolve_model_case_insensitive() {
        let reg = default_reg();
        assert_eq!(
            reg.resolve_model("POWERFUL", "anthropic"),
            "claude-opus-5-5"
        );
        assert_eq!(
            reg.resolve_model("Standard", "anthropic"),
            "claude-sonnet-5-5"
        );
    }

    #[test]
    fn test_resolve_model_unknown_tier_for_provider() {
        let reg = default_reg();
        // Grok cheap is the cheaper 1M-context grok-4.3
        assert_eq!(reg.resolve_model("cheap", "grok"), "grok-4.3");
        // Unknown provider returns tier name as-is
        assert_eq!(reg.resolve_model("cheap", "nonexistent"), "cheap");
    }

    #[test]
    fn test_classify_known_models() {
        let reg = default_reg();
        assert_eq!(reg.classify("claude-opus-4-6"), ModelTier::Powerful);
        assert_eq!(reg.classify("claude-sonnet-4-6"), ModelTier::Standard);
        assert_eq!(reg.classify("claude-haiku-4-6"), ModelTier::Cheap);
        // grok-4.7 is both powerful and standard — classify returns highest
        assert_eq!(reg.classify("grok-4.7"), ModelTier::Powerful);
        assert_eq!(reg.classify("grok-4.3"), ModelTier::Cheap);
        assert_eq!(reg.classify("gemini-3.1-pro-preview"), ModelTier::Powerful);
        assert_eq!(reg.classify("gemini-3.8-flash"), ModelTier::Standard);
        assert_eq!(reg.classify("gemini-3.5-flash-lite"), ModelTier::Cheap);
    }

    #[test]
    fn test_classify_openrouter_models() {
        let reg = default_reg();
        assert_eq!(
            reg.classify("anthropic/claude-sonnet-4.6"),
            ModelTier::Standard
        );
        // deepseek-v4.1-flash appears as both standard and cheap — classify returns highest
        assert_eq!(
            reg.classify("deepseek/deepseek-v4.1-flash"),
            ModelTier::Standard
        );
        assert_eq!(
            reg.classify("deepseek/deepseek-v4-pro"),
            ModelTier::Powerful
        );
    }

    #[test]
    fn test_classify_heuristic_fallback() {
        let reg = default_reg();
        assert_eq!(reg.classify("claude-haiku-99"), ModelTier::Cheap);
        assert_eq!(reg.classify("claude-opus-99"), ModelTier::Powerful);
        assert_eq!(reg.classify("some-random-model"), ModelTier::Standard);
    }

    #[test]
    fn test_resolve_equivalent() {
        let reg = default_reg();
        assert_eq!(
            reg.resolve_equivalent("claude-opus-4-6", "gemini"),
            Some("gemini-3.1-pro-preview".to_string())
        );
        assert_eq!(
            reg.resolve_equivalent("claude-sonnet-4-6", "grok"),
            Some("grok-4.7".to_string())
        );
        // Grok cheap maps to grok-4.3
        assert_eq!(
            reg.resolve_equivalent("claude-haiku-4-6", "grok"),
            Some("grok-4.3".to_string())
        );
    }

    #[test]
    fn test_resolve_healthy_equivalent_with_fallback() {
        let reg = default_reg();
        // Haiku → Grok cheap resolves to grok-4.3
        assert_eq!(
            reg.resolve_healthy_equivalent("claude-haiku-4-6", &["grok"]),
            Some("grok-4.3".to_string())
        );
    }

    #[test]
    fn test_all_providers_have_all_tiers() {
        let reg = default_reg();
        for tier in &[ModelTier::Cheap, ModelTier::Standard, ModelTier::Powerful] {
            for key in &[
                "anthropic",
                "grok",
                "gemini",
                "openrouter.anthropic",
                "openrouter.deepseek",
                "openrouter.google",
                "openrouter.qwen",
                "openrouter.kimi",
                "zai",
            ] {
                assert!(
                    reg.resolve_for_tier(*tier, key).is_some(),
                    "Missing {} tier for provider {}",
                    tier.as_str(),
                    key
                );
            }
        }
    }

    #[test]
    fn test_parse_provider_tier_key() {
        let (name, key) = parse_provider_tier_key("anthropic").unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(key, "anthropic");

        for removed in ["claude-max", "max", "Claude-Max:sonnet", "oauth:opus"] {
            let err = parse_provider_tier_key(removed).unwrap_err().to_string();
            assert!(err.contains("was removed"), "{removed}: {err}");
        }

        let (name, key) = parse_provider_tier_key("openrouter:deepseek").unwrap();
        assert_eq!(name, "openrouter");
        assert_eq!(key, "openrouter.deepseek");

        let (name, key) = parse_provider_tier_key("openrouter:anthropic").unwrap();
        assert_eq!(name, "openrouter");
        assert_eq!(key, "openrouter.anthropic");

        let (name, key) = parse_provider_tier_key("gemini").unwrap();
        assert_eq!(name, "gemini");
        assert_eq!(key, "gemini");

        let (name, key) = parse_provider_tier_key("grok").unwrap();
        assert_eq!(name, "grok");
        assert_eq!(key, "grok");

        let (name, key) = parse_provider_tier_key("vllm").unwrap();
        assert_eq!(name, "vllm");
        assert_eq!(key, "vllm");

        let (name, key) = parse_provider_tier_key("zai").unwrap();
        assert_eq!(name, "zai");
        assert_eq!(key, "zai");
    }

    #[test]
    fn test_parse_provider_bare_openrouter_is_error() {
        assert!(parse_provider_tier_key("openrouter").is_err());
    }

    #[test]
    fn test_merge_toml() {
        let reg = default_reg()
            .merge_toml(
                r#"
[grok]
cheap = "grok-mini"

[openrouter.meta]
standard = "meta-llama/llama-3.3-70b"
"#,
            )
            .unwrap();

        // Override took effect
        assert_eq!(reg.resolve_model("cheap", "grok"), "grok-mini");
        // New provider added
        assert_eq!(
            reg.resolve_model("standard", "openrouter.meta"),
            "meta-llama/llama-3.3-70b"
        );
        // Existing defaults preserved
        assert_eq!(
            reg.resolve_model("powerful", "anthropic"),
            "claude-opus-5-5"
        );
    }

    #[test]
    fn test_with_equivalences() {
        let reg = default_reg().with_equivalences(&{
            let mut m = HashMap::new();
            m.insert(
                "powerful".to_string(),
                vec!["grok:grok-4-custom".to_string()],
            );
            m
        });
        assert_eq!(reg.resolve_model("powerful", "grok"), "grok-4-custom");
    }

    #[test]
    fn from_cell_applies_equivalences_and_defaults_when_empty() {
        let mut m = HashMap::new();
        m.insert(
            "powerful".to_string(),
            vec!["grok:grok-from-cell".to_string()],
        );
        let cell: EquivalencesCell =
            std::sync::Arc::new(parking_lot::RwLock::new(Some(std::sync::Arc::new(m))));
        assert_eq!(
            TierRegistry::from_cell(&cell).resolve_model("powerful", "grok"),
            "grok-from-cell"
        );

        let empty: EquivalencesCell = std::sync::Arc::new(parking_lot::RwLock::new(None));
        assert_eq!(
            TierRegistry::from_cell(&empty).resolve_model("powerful", "grok"),
            TierRegistry::new(None).resolve_model("powerful", "grok")
        );
    }

    #[test]
    fn test_resolve_routed_plain_tier() {
        let reg = default_reg();
        // Plain tier uses default provider
        let (key, model) = reg.resolve_routed_model("standard", "anthropic");
        assert_eq!(key, None);
        assert_eq!(model, "claude-sonnet-5-5");
    }

    #[test]
    fn test_resolve_routed_provider_tier() {
        let reg = default_reg();
        // gemini:cheap → routes to gemini provider
        let (key, model) = reg.resolve_routed_model("gemini:cheap", "anthropic");
        assert_eq!(key, Some("gemini".to_string()));
        assert_eq!(model, "gemini-3.5-flash-lite");

        // anthropic:powerful → same as default, no routing needed
        let (key, model) = reg.resolve_routed_model("anthropic:powerful", "anthropic");
        assert_eq!(key, None);
        assert_eq!(model, "claude-opus-5-5");

        // grok:standard → routes to grok
        let (key, model) = reg.resolve_routed_model("grok:standard", "anthropic");
        assert_eq!(key, Some("grok".to_string()));
        assert_eq!(model, "grok-4.7");
    }

    #[test]
    fn test_resolve_routed_openrouter_upstream() {
        let reg = default_reg();
        // openrouter:deepseek:standard → routes via openrouter
        let (key, model) = reg.resolve_routed_model("openrouter:deepseek:standard", "anthropic");
        assert_eq!(key, Some("openrouter".to_string()));
        assert_eq!(model, "deepseek/deepseek-v4.1-flash");

        // openrouter:google:cheap → routes via openrouter
        let (key, model) = reg.resolve_routed_model("openrouter:google:cheap", "anthropic");
        assert_eq!(key, Some("openrouter".to_string()));
        assert_eq!(model, "google/gemini-3.5-flash-lite");
    }

    #[test]
    fn test_resolve_routed_concrete_model() {
        let reg = default_reg();
        // Concrete model name passes through
        let (key, model) = reg.resolve_routed_model("claude-sonnet-4-6", "anthropic");
        assert_eq!(key, None);
        assert_eq!(model, "claude-sonnet-4-6");
    }

    #[test]
    fn test_resolve_routed_same_as_default() {
        let reg = default_reg();
        // When routed provider matches default, no routing key needed
        let (key, model) = reg.resolve_routed_model("gemini:powerful", "gemini");
        assert_eq!(key, None);
        assert_eq!(model, "gemini-3.1-pro-preview");
    }

    #[test]
    fn test_budget_to_thinking_level() {
        assert_eq!(budget_to_thinking_level(0), "minimal");
        assert_eq!(budget_to_thinking_level(1000), "low");
        assert_eq!(budget_to_thinking_level(2000), "low");
        assert_eq!(budget_to_thinking_level(5000), "medium");
        assert_eq!(budget_to_thinking_level(8000), "medium");
        assert_eq!(budget_to_thinking_level(10000), "high");
        assert_eq!(budget_to_thinking_level(50000), "high");
    }

    #[test]
    fn test_provider_inference() {
        assert_eq!(
            Provider::from_model_str("claude-opus-4-6"),
            (Provider::Anthropic, "claude-opus-4-6")
        );
        assert_eq!(
            Provider::from_model_str("grok:grok-4"),
            (Provider::Grok, "grok-4")
        );
        assert_eq!(
            Provider::from_model_str("gemini:gemini-2.5-pro"),
            (Provider::Gemini, "gemini-2.5-pro")
        );
        assert_eq!(
            Provider::from_model_str("zai:glm-5.1"),
            (Provider::Zai, "glm-5.1")
        );
        assert_eq!(
            Provider::from_model_str("glm-5.1"),
            (Provider::Zai, "glm-5.1")
        );
    }

    #[test]
    fn test_provider_from_name_openrouter_tier_keys() {
        assert_eq!(
            Provider::from_name("openrouter.deepseek"),
            Some(Provider::OpenRouter)
        );
        assert_eq!(
            Provider::from_name("openrouter.anthropic"),
            Some(Provider::OpenRouter)
        );
    }
}
