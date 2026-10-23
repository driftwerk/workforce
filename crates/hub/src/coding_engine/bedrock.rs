//! Claude Code billed per token through Bedrock: the same CLI as the
//! subscription engine, pointed at Bedrock with credentials the hub mints for
//! the round from an invoke-only role, so what lands in a scope expires with
//! the round. The models are Anthropic's on Bedrock, resolved through the
//! `[bedrock.anthropic]` tier table, and the rounds are priced from their
//! token counts at Bedrock rates because the CLI's own cost figure is an
//! Anthropic-list estimate.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aws_config::sts::AssumeRoleProvider;
use aws_config::BehaviorVersion;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use eyre::{bail, eyre, Result, WrapErr};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;
use tracing::info;
use workforce_agent::llm::bedrock::resolve_region;
use workforce_agent::llm::tiers::{ModelTier, TierRegistry};
use workforce_agent::runner::ProgressEvent;
use workforce_common::settings::CodingBedrockConfig;

use super::claude_code::{claude_limit_reached, claude_model_choices, config_for};
use super::{CliRoundResult, CodingEngine, CodingRound, ModelChoices, Refusal, RoundContext};
use crate::claude_code::{
    self, BedrockCredentials, BedrockSession, ClaudeAuth, ClaudeCodeConfig, CliHealth, ModelPins,
};
use crate::coding_sandbox::CodingSandbox;
use crate::mcp_turn::ClientTimeout;

pub const LABEL: &str = "claude-code:bedrock";
const TIER_KEY: &str = "bedrock.anthropic";
/// How much longer than the round its credential lives, so a turn in flight
/// at the deadline still completes.
const CREDENTIAL_MARGIN: Duration = Duration::from_secs(300);
const STS_MIN_SESSION: Duration = Duration::from_secs(900);
const SESSION_NAME_MAX: usize = 64;
/// Cross-region inference-profile prefixes a model id may already carry.
const PROFILE_PREFIXES: &[&str] = &["eu", "us", "apac", "global", "jp", "au", "ca", "us-gov"];

/// Mints credentials for one round.
#[async_trait]
pub trait CredentialMinter: Send + Sync {
    async fn mint(&self, session_name: &str, lifetime: Duration) -> Result<BedrockCredentials>;
}

/// `sts:AssumeRole` on the invoke role, from the hub's own identity.
pub struct StsMinter {
    role_arn: String,
    region: String,
    base: OnceCell<aws_config::SdkConfig>,
}

impl StsMinter {
    pub fn new(role_arn: String, region: String) -> Self {
        Self {
            role_arn,
            region,
            base: OnceCell::new(),
        }
    }

    async fn base(&self) -> &aws_config::SdkConfig {
        self.base
            .get_or_init(|| async {
                aws_config::defaults(BehaviorVersion::latest())
                    .region(aws_config::Region::new(self.region.clone()))
                    .load()
                    .await
            })
            .await
    }
}

#[async_trait]
impl CredentialMinter for StsMinter {
    async fn mint(&self, session_name: &str, lifetime: Duration) -> Result<BedrockCredentials> {
        let base = self.base().await;
        let source = base.credentials_provider().ok_or_else(|| {
            eyre!("the hub has no AWS credentials to assume the Bedrock role from")
        })?;
        let provider = AssumeRoleProvider::builder(&self.role_arn)
            .session_name(session_name)
            .session_length(lifetime)
            .configure(base)
            .build_from_provider(SharedCredentialsProvider::new(source))
            .await;
        let credentials = provider
            .provide_credentials()
            .await
            .wrap_err_with(|| format!("assuming the Bedrock invoke role {}", self.role_arn))?;
        Ok(BedrockCredentials {
            access_key_id: credentials.access_key_id().to_string(),
            secret_access_key: credentials.secret_access_key().to_string(),
            session_token: credentials.session_token().map(str::to_string),
        })
    }
}

struct Setup {
    minter: Arc<dyn CredentialMinter>,
    region: String,
    prefix: String,
    max_session: Duration,
}

#[derive(Default)]
pub struct ClaudeCodeBedrockEngine {
    setup: Option<Setup>,
}

impl ClaudeCodeBedrockEngine {
    /// The engine `[coding_session.bedrock]` describes; unconfigured when it
    /// names no role, and then every preflight says so.
    pub fn from_config(config: &CodingBedrockConfig) -> Self {
        Self::with_minter(config, |role_arn, region| {
            Arc::new(StsMinter::new(role_arn, region))
        })
    }

    fn with_minter(
        config: &CodingBedrockConfig,
        minter: impl FnOnce(String, String) -> Arc<dyn CredentialMinter>,
    ) -> Self {
        let setup = config.assume_role_arn.clone().map(|role_arn| {
            let region = resolve_region(config.region.as_deref());
            let prefix = config
                .region_prefix
                .clone()
                .unwrap_or_else(|| region_prefix(&region));
            Setup {
                minter: minter(role_arn, region.clone()),
                region,
                prefix,
                max_session: config.effective_max_session(),
            }
        });
        Self { setup }
    }

    async fn run_round(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        started_at: chrono::DateTime<chrono::Utc>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let setup = self.setup.as_ref().ok_or_else(|| eyre!(unconfigured()))?;
        let tiers = ctx.service.tier_registry();
        let model = resolve_model(round.model.as_deref(), &tiers)?;
        let pins = pins(&tiers, &setup.prefix)?;
        let (lifetime, timeout) = credential_lifetime(round.timeout, setup.max_session);
        let who = ctx.identity.unwrap_or(ctx.service.agent_id.as_str());
        let round_id = uuid::Uuid::new_v4().simple().to_string();
        let credentials = setup
            .minter
            .mint(&session_name(who, &round_id[..8]), lifetime)
            .await?;
        if timeout < round.timeout {
            info!(
                target: "cc",
                wanted_secs = round.timeout.as_secs(),
                allowed_secs = timeout.as_secs(),
                "Bedrock round cut to what its credential covers"
            );
        }
        let session = BedrockSession {
            region: setup.region.clone(),
            credentials,
            pins,
        };
        let mcp = ctx
            .round_tools(
                sandbox.as_ref(),
                cancel.as_ref(),
                ClientTimeout::CLAUDE_CODE,
            )
            .map(|tools| tools.with_model(model.clone()));
        let config = placed(
            config_for(sandbox.as_ref(), round, mcp, ClaudeAuth::Bedrock(session)),
            profile_id(&model, &setup.prefix),
            timeout,
        );
        let result = claude_code::run_coding_round(
            &config,
            ctx.service.vault.as_deref(),
            ctx.service.global_vault.as_deref(),
            cancel,
            progress,
        )
        .await?;
        let ledger_model = format!("{LABEL}:{model}");
        let cost_usd = ctx.service.cost_tracker.calculate_cost(
            &ledger_model,
            result.input_tokens,
            result.output_tokens,
            result.cache_creation_tokens,
            result.cache_read_tokens,
        );
        let result = CliRoundResult {
            engine: LABEL.to_string(),
            model: Some(model),
            cost_usd,
            ..result
        };
        ctx.record(LABEL, &ledger_model, &result, started_at, None);
        Ok(result)
    }
}

fn unconfigured() -> String {
    "Claude Code on Bedrock is not set up on this hub: set [coding_session.bedrock] \
     assume_role_arn to a role that may invoke Bedrock models."
        .to_string()
}

/// The cross-region inference-profile prefix for a region, e.g. `eu` for
/// `eu-west-2`; Asia-Pacific regions use `apac`.
fn region_prefix(region: &str) -> String {
    match region.split('-').next().unwrap_or(region) {
        "ap" => "apac".to_string(),
        other => other.to_string(),
    }
}

/// The inference-profile id for a model, prefixing a bare id and leaving an
/// already-qualified one alone.
fn profile_id(model: &str, prefix: &str) -> String {
    if model.starts_with("arn:") || bare_id(model) != model {
        model.to_string()
    } else {
        format!("{prefix}.{model}")
    }
}

/// The model id without any inference-profile prefix: the pricing key.
fn bare_id(model: &str) -> &str {
    PROFILE_PREFIXES
        .iter()
        .find_map(|prefix| {
            model
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('.'))
        })
        .unwrap_or(model)
}

/// Fable on Bedrock: a global cross-region inference profile with no
/// regional (`eu.`) variant, so it is pinned fully qualified and
/// [`profile_id`] leaves it alone. Invoking it needs the account's data
/// retention mode set to `provider_data_share`.
const FABLE_PROFILE: &str = "global.anthropic.claude-fable-5-1";

/// The tier a Claude Code model alias stands for. `fable` is not a tier:
/// it names [`FABLE_PROFILE`] and is resolved before this is asked.
pub(super) fn alias_tier(spec: &str) -> Option<ModelTier> {
    match spec {
        "opus" => Some(ModelTier::Powerful),
        "sonnet" => Some(ModelTier::Standard),
        "haiku" => Some(ModelTier::Cheap),
        other => ModelTier::from_name(other),
    }
}

fn tier_model(tiers: &TierRegistry, tier: ModelTier) -> Result<String> {
    tiers
        .resolve_for_tier(tier, TIER_KEY)
        .ok_or_else(|| eyre!("no {} model in the [{TIER_KEY}] tier table", tier.as_str()))
}

/// The bare Bedrock model id for a round's model: a CLI alias, a tier, a
/// `bedrock.anthropic:<tier>` spec, or a concrete id, with or without a
/// `bedrock:` routing prefix. Anything else is refused, because Claude Code
/// runs nothing but Anthropic models on Bedrock.
fn resolve_model(spec: Option<&str>, tiers: &TierRegistry) -> Result<String> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return tier_model(tiers, ModelTier::Standard);
    };
    if spec == "fable" {
        return Ok(FABLE_PROFILE.to_string());
    }
    if let Some(tier) = alias_tier(spec) {
        return tier_model(tiers, tier);
    }
    if let Some((head, tier)) = spec
        .rsplit_once(':')
        .and_then(|(head, tail)| ModelTier::from_name(tail).map(|tier| (head, tier)))
    {
        return match head {
            "bedrock" | "bedrock.anthropic" | "bedrock:anthropic" => tier_model(tiers, tier),
            other => bail!("Claude Code on Bedrock cannot run {spec:?}: it routes to {other}"),
        };
    }
    let bare = bare_id(spec.strip_prefix("bedrock:").unwrap_or(spec));
    if !bare.starts_with("anthropic.") {
        bail!("Claude Code on Bedrock runs Anthropic models only; {spec:?} is not one");
    }
    Ok(bare.to_string())
}

fn pins(tiers: &TierRegistry, prefix: &str) -> Result<ModelPins> {
    Ok(ModelPins {
        opus: profile_id(&tier_model(tiers, ModelTier::Powerful)?, prefix),
        sonnet: profile_id(&tier_model(tiers, ModelTier::Standard)?, prefix),
        haiku: profile_id(&tier_model(tiers, ModelTier::Cheap)?, prefix),
    })
}

/// How long the round's credential lives and how long the round may run on
/// it: the credential outlives the round by [`CREDENTIAL_MARGIN`] within what
/// STS and the role allow, and a round the role cannot cover is cut to fit.
fn credential_lifetime(timeout: Duration, max_session: Duration) -> (Duration, Duration) {
    let lifetime =
        (timeout + CREDENTIAL_MARGIN).clamp(STS_MIN_SESSION, max_session.max(STS_MIN_SESSION));
    (lifetime, timeout.min(lifetime - CREDENTIAL_MARGIN))
}

/// An STS session name that says whose round it was, in STS's charset.
fn session_name(who: &str, round: &str) -> String {
    format!("workforce-coding-{who}-{round}")
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '=' | ',' | '.' | '@' | '-')
        })
        .take(SESSION_NAME_MAX)
        .collect()
}

fn placed(config: ClaudeCodeConfig, model: String, timeout: Duration) -> ClaudeCodeConfig {
    ClaudeCodeConfig {
        model: Some(model),
        timeout,
        ..config
    }
}

#[async_trait]
impl CodingEngine for ClaudeCodeBedrockEngine {
    fn label(&self) -> &'static str {
        LABEL
    }

    fn metered(&self) -> bool {
        true
    }

    fn preflight(&self, _: &RoundContext<'_>) -> Result<(), Refusal> {
        self.setup
            .as_ref()
            .map(drop)
            .ok_or_else(|| Refusal::no_token(unconfigured()))
    }

    fn limit_reached(&self, text: &str) -> bool {
        claude_limit_reached(text)
    }

    fn model_choices(&self) -> ModelChoices {
        claude_model_choices()
    }

    fn model_for(&self, requested: Option<&str>, ctx: &RoundContext<'_>) -> Option<String> {
        resolve_model(requested, &ctx.service.tier_registry()).ok()
    }

    async fn cli_health(&self) -> Option<CliHealth> {
        Some(claude_code::probe_cli().await)
    }

    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        ctx: &RoundContext<'_>,
        cancel: Option<CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        let started_at = chrono::Utc::now();
        self.run_round(sandbox, round, ctx, started_at, cancel, progress)
            .await
            .inspect_err(|e| ctx.record_error(self, round, started_at, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use workforce_agent::llm::DEFAULT_PRICING_TOML;
    use workforce_common::cost::load_pricing_toml;

    fn tiers() -> TierRegistry {
        TierRegistry::new(None)
    }

    #[test]
    fn a_region_names_its_profile_prefix() {
        assert_eq!(region_prefix("eu-west-2"), "eu");
        assert_eq!(region_prefix("us-east-1"), "us");
        assert_eq!(region_prefix("ap-southeast-2"), "apac");
    }

    #[test]
    fn profile_ids_are_prefixed_once() {
        assert_eq!(
            profile_id("anthropic.claude-sonnet-5", "eu"),
            "eu.anthropic.claude-sonnet-5"
        );
        assert_eq!(
            profile_id("us.anthropic.claude-sonnet-5", "eu"),
            "us.anthropic.claude-sonnet-5"
        );
        let arn = "arn:aws:bedrock:eu-west-2:1:application-inference-profile/x";
        assert_eq!(profile_id(arn, "eu"), arn);
        assert_eq!(
            bare_id("global.anthropic.claude-opus-4-8"),
            "anthropic.claude-opus-4-8"
        );
        assert_eq!(
            bare_id("anthropic.claude-opus-4-8"),
            "anthropic.claude-opus-4-8"
        );
    }

    #[test]
    fn the_round_model_resolves_to_an_anthropic_model_on_bedrock() {
        let tiers = tiers();
        let standard = tiers
            .resolve_for_tier(ModelTier::Standard, TIER_KEY)
            .unwrap();
        let powerful = tiers
            .resolve_for_tier(ModelTier::Powerful, TIER_KEY)
            .unwrap();
        assert_eq!(resolve_model(None, &tiers).unwrap(), standard);
        assert_eq!(resolve_model(Some("sonnet"), &tiers).unwrap(), standard);
        assert_eq!(resolve_model(Some("opus"), &tiers).unwrap(), powerful);
        assert_eq!(resolve_model(Some("powerful"), &tiers).unwrap(), powerful);
        assert_eq!(
            resolve_model(Some("bedrock.anthropic:standard"), &tiers).unwrap(),
            standard
        );
        assert_eq!(
            resolve_model(Some("eu.anthropic.claude-sonnet-4-6"), &tiers).unwrap(),
            "anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            resolve_model(Some("bedrock:anthropic.claude-sonnet-4-6"), &tiers).unwrap(),
            "anthropic.claude-sonnet-4-6"
        );
        assert_eq!(
            resolve_model(Some("anthropic.claude-sonnet-4-5-20250929-v1:0"), &tiers).unwrap(),
            "anthropic.claude-sonnet-4-5-20250929-v1:0"
        );
        for other in [
            "openrouter:anthropic:cheap",
            "deepseek.v3.2",
            "gemini:gemini-3-flash",
        ] {
            let err = resolve_model(Some(other), &tiers).unwrap_err().to_string();
            assert!(err.contains(other), "{other}: {err}");
        }
    }

    #[test]
    fn fable_is_the_global_profile_and_keeps_no_regional_prefix() {
        let tiers = tiers();
        let fable = resolve_model(Some("fable"), &tiers).unwrap();
        assert_eq!(fable, "global.anthropic.claude-fable-5-1");
        assert_eq!(profile_id(&fable, "eu"), fable, "already qualified");
        assert_eq!(
            bare_id(&fable),
            "anthropic.claude-fable-5-1",
            "the pricing key"
        );
        // Every key a Fable round can be recorded under is priced, at
        // Fable's own sticker price rather than the unknown-model default.
        let pricing = load_pricing_toml(DEFAULT_PRICING_TOML).unwrap();
        let round_key = format!("{LABEL}:{}", bare_id(&fable));
        for key in [fable.as_str(), bare_id(&fable), round_key.as_str()] {
            assert!(pricing.contains_key(key), "{key} has no pricing row");
            assert_eq!(
                pricing[key].input_per_million, 10.0,
                "{key} is Fable's price"
            );
            assert_eq!(
                pricing[key].output_per_million, 50.0,
                "{key} is Fable's price"
            );
        }
        assert_ne!(
            fable,
            tiers
                .resolve_for_tier(ModelTier::Powerful, TIER_KEY)
                .unwrap(),
            "fable is no longer an alias for the powerful tier"
        );
    }

    #[test]
    fn every_alias_pin_is_a_profile_in_the_region() {
        let pins = pins(&tiers(), "eu").unwrap();
        for pin in [&pins.opus, &pins.sonnet, &pins.haiku] {
            assert!(pin.starts_with("eu.anthropic."), "{pin}");
        }
    }

    #[test]
    fn the_credential_outlives_the_round_within_what_the_role_allows() {
        let hour = Duration::from_secs(3600);
        let (lifetime, timeout) = credential_lifetime(Duration::from_secs(1500), hour);
        assert_eq!(lifetime, Duration::from_secs(1800));
        assert_eq!(timeout, Duration::from_secs(1500));

        let (lifetime, timeout) = credential_lifetime(Duration::from_secs(60), hour);
        assert_eq!(lifetime, STS_MIN_SESSION, "STS's floor");
        assert_eq!(timeout, Duration::from_secs(60));

        let (lifetime, timeout) = credential_lifetime(Duration::from_secs(7200), hour);
        assert_eq!(lifetime, hour, "the role's ceiling");
        assert_eq!(timeout, hour - CREDENTIAL_MARGIN, "the round is cut to fit");
    }

    #[test]
    fn session_names_fit_sts() {
        let name = session_name("kingsley@harbor.example", "0123abcd");
        assert_eq!(name, "workforce-coding-kingsley@harbor.example-0123abcd");
        let long = session_name(&"x".repeat(100), "0123abcd");
        assert_eq!(long.len(), SESSION_NAME_MAX);
        assert!(!session_name("a b/c", "1").contains([' ', '/']));
    }

    #[test]
    fn placing_a_round_switches_its_auth_model_and_clock() {
        let round = CodingRound {
            model: Some("opus".into()),
            ..CodingRound::one_shot("t".into(), Duration::from_secs(7200))
        };
        let session = BedrockSession {
            region: "eu-west-2".into(),
            credentials: BedrockCredentials {
                access_key_id: "AKIA".into(),
                secret_access_key: "s3cr3t-value".into(),
                session_token: Some("token".into()),
            },
            pins: pins(&tiers(), "eu").unwrap(),
        };
        let config = placed(
            config_for(
                &super::super::claude_code::tests::StubSandbox,
                &round,
                None,
                ClaudeAuth::Bedrock(session),
            ),
            "eu.anthropic.claude-opus-4-8".into(),
            Duration::from_secs(3300),
        );
        assert_eq!(
            config.model.as_deref(),
            Some("eu.anthropic.claude-opus-4-8")
        );
        assert_eq!(config.timeout, Duration::from_secs(3300));
        assert!(matches!(config.auth, ClaudeAuth::Bedrock(_)));
        assert_eq!(config.workdir, PathBuf::from("/work/rails"));
        assert!(!format!("{:?}", config.auth).contains("s3cr3t-value"));
    }

    #[test]
    fn an_unconfigured_engine_says_what_to_set() {
        let engine = ClaudeCodeBedrockEngine::from_config(&CodingBedrockConfig::default());
        assert!(engine.setup.is_none());
        assert!(engine.metered());
        assert!(unconfigured().contains("assume_role_arn"));
    }

    #[test]
    fn a_configured_engine_derives_its_prefix_from_the_region() {
        let config = CodingBedrockConfig {
            assume_role_arn: Some("arn:aws:iam::1:role/r".into()),
            region: Some("eu-west-2".into()),
            ..Default::default()
        };
        let engine = ClaudeCodeBedrockEngine::from_config(&config);
        let setup = engine.setup.as_ref().unwrap();
        assert_eq!(setup.prefix, "eu");
        assert_eq!(setup.region, "eu-west-2");
        assert_eq!(setup.max_session, Duration::from_secs(3600));
        let pinned = ClaudeCodeBedrockEngine::from_config(&CodingBedrockConfig {
            region_prefix: Some("global".into()),
            ..config
        });
        assert_eq!(pinned.setup.unwrap().prefix, "global");
    }

    /// Every model name the subscription engine offers is priced under the
    /// key its rounds are recorded with, so a subscription round never falls
    /// to the unknown-model default.
    #[test]
    fn every_claude_code_alias_is_priced_as_a_subscription_round() {
        let pricing = load_pricing_toml(DEFAULT_PRICING_TOML).unwrap();
        for alias in super::super::claude_code::claude_model_choices()
            .models
            .iter()
            .chain(std::iter::once(&"fable".to_string()))
        {
            let key = format!("{}:{alias}", super::super::claude_code::LABEL);
            assert!(pricing.contains_key(&key), "{key} has no pricing row");
        }
    }

    /// Every model the tier table can hand a round is priced under the key
    /// its rounds are recorded with.
    #[test]
    fn every_tier_model_is_priced_as_a_claude_code_bedrock_round() {
        let pricing = load_pricing_toml(DEFAULT_PRICING_TOML).unwrap();
        let tiers = tiers();
        for tier in [ModelTier::Cheap, ModelTier::Standard, ModelTier::Powerful] {
            let model = tier_model(&tiers, tier).unwrap();
            let key = format!("{LABEL}:{model}");
            assert!(pricing.contains_key(&key), "{key} has no pricing row");
            assert_eq!(
                pricing[&key].input_per_million, pricing[&model].input_per_million,
                "{key} is priced like {model}"
            );
        }
    }
}
