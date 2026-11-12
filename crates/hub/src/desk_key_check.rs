//! A key Desk saves is tried first: one call as small as the provider
//! takes (a one-token answer, or a word spoken), so "Connected" means the
//! provider answered. When it didn't, the person is told why in plain words
//! and what to do: the key is wrong, the account has no credit, the region
//! doesn't offer the model.
//!
//! Nothing here ever logs or repeats what the provider said back, nor the
//! key: providers quote parts of a refused key ("Incorrect API key provided:
//! sk-…abcd"), so a refusal is told from its status and kind alone.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use workforce_agent::llm::failover::{classify_error, ErrorKind};
use workforce_agent::llm::provider::{create_provider_with, AccountExtras, ProviderKind};
use workforce_agent::llm::speech::{provider_from_config, SpeechRequest};
use workforce_agent::llm::types::{ContentBlock, LlmMessage, Role};
use workforce_common::secret::KeySlot;
use workforce_common::settings::LlmSpeechConfig;
use workforce_desk::local::{self, Server};

/// How long a test call may take before Desk says the provider didn't
/// answer.
pub(crate) const DEADLINE: Duration = Duration::from_secs(20);

/// What a test call was made on, for the words of a refusal.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tried<'a> {
    /// As the person knows the provider: "Anthropic", "Amazon Bedrock".
    pub name: &'a str,
    /// The provider kind, for one that thinks; none for a voice.
    pub kind: Option<ProviderKind>,
    /// Where a new key is made, as Desk's form says it.
    pub hint: &'a str,
    pub model: Option<&'a str>,
    /// Bedrock's region, or Vertex's location.
    pub region: Option<&'a str>,
    pub project: Option<&'a str>,
    /// Signed in with an AWS profile or a Google login, not a pasted key.
    pub login: bool,
}

/// Why the test call didn't pass, in the person's words, from what the call
/// failed with. `None` when the failure still shows the key works: a 429
/// that isn't about credit (the provider knew the key well enough to limit
/// it), or a failure after a provider answered (an answer this client
/// couldn't read is still one).
pub(crate) fn refusal(tried: &Tried<'_>, error: &str) -> Option<String> {
    let lower = error.to_lowercase();
    let name = tried.name;
    let bedrock = tried.kind == Some(ProviderKind::Bedrock);
    let vertex = tried.kind == Some(ProviderKind::VertexAnthropic);
    let region = tried.region.unwrap_or("your region");
    let model = tried.model.unwrap_or("its model");
    let status = status_of(&lower);
    if lower.contains(TIMED_OUT) {
        return Some(format!(
            "{name} didn't answer within {} s, so the key wasn't checked. Save it again in a \
             moment.",
            DEADLINE.as_secs()
        ));
    }
    if status.is_none() && unreachable(&lower) {
        return Some(match bedrock {
            true => format!(
                "Desk couldn't reach Amazon Bedrock in {region}. Check the region is written \
                 like us-east-1 and offers Bedrock, and that this computer is online."
            ),
            false => format!(
                "Desk couldn't reach {name} to check the key. Check this computer is online, \
                 then save it again."
            ),
        });
    }
    if no_credit(&lower) || classify_error(&lower) == ErrorKind::Billing {
        return Some(format!(
            "{name} knows the key, but the account has no credit left. {}, then save it again.",
            top_up(tried)
        ));
    }
    if bedrock {
        if let Some(said) = bedrock_refusal(&lower, tried, region, model) {
            return Some(said);
        }
    }
    if vertex {
        if let Some(said) = vertex_refusal(&lower, status, tried, model) {
            return Some(said);
        }
    }
    if wrong_key(&lower) {
        return Some(wrong(tried));
    }
    match classify_error(&lower) {
        ErrorKind::Auth => Some(wrong(tried)),
        // The provider knew the key well enough to limit it.
        ErrorKind::RateLimited => None,
        ErrorKind::ServerError | ErrorKind::Overloaded | ErrorKind::Timeout => Some(format!(
            "{name} is having trouble right now, so the key couldn't be checked. Save it again \
             in a moment."
        )),
        // Any other refusal came after the provider knew the key (a model
        // that won't answer in one token, an upstream behind a router), and
        // one with no status came after it answered: the key passes.
        _ => None,
    }
}

/// Whether a failed test call already says what's wrong: the key, the
/// account's credit, a limit, or reaching the provider at all. Anything
/// else is checked again with the provider's own key check.
fn decisive(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains(TIMED_OUT)
        || (status_of(&lower).is_none() && unreachable(&lower))
        || no_credit(&lower)
        || wrong_key(&lower)
        || matches!(
            classify_error(&lower),
            ErrorKind::Auth | ErrorKind::Billing | ErrorKind::RateLimited
        )
}

/// The marker [`with_deadline`] fails with.
const TIMED_OUT: &str = "the test call ran out of time";

/// `call`, or [`TIMED_OUT`] once [`DEADLINE`] passes.
pub(crate) async fn with_deadline(
    call: impl std::future::Future<Output = eyre::Result<()>>,
) -> Result<(), String> {
    match tokio::time::timeout(DEADLINE, call).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err(TIMED_OUT.into()),
    }
}

/// The HTTP status a provider's error names, in the forms the hub's clients
/// write: `… error (401 …)`, `… returned 402 …`, `HTTP 403 …`.
fn status_of(lower: &str) -> Option<u16> {
    for marker in [
        " error (",
        "returned ",
        "http ",
        "status: ",
        "status ",
        "vertex ai ",
    ] {
        for (at, m) in lower.match_indices(marker) {
            let rest = &lower[at + m.len()..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 3 {
                if let Ok(code @ 400..=599) = rest[..3].parse::<u16>() {
                    return Some(code);
                }
            }
        }
    }
    None
}

fn unreachable(lower: &str) -> bool {
    [
        "request failed",
        "error sending request",
        "network error",
        "dns error",
        "failed to lookup address",
        "connection refused",
        "connection reset",
        "could not connect",
        "tcp connect error",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

fn no_credit(lower: &str) -> bool {
    [
        "credit balance",
        "insufficient_quota",
        "insufficient balance",
        "insufficient credit",
        "any credits",
        "no credits",
        "out of credits",
        "exceeded your current quota",
        "billing",
        "payment required",
        "wants payment",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

fn wrong_key(lower: &str) -> bool {
    [
        "api key not valid",
        "invalid api key",
        "invalid_api_key",
        "invalid x-api-key",
        "incorrect api key",
        "invalid authentication",
        "authentication_error",
        "invalid token",
        "unauthenticated",
    ]
    .iter()
    .any(|s| lower.contains(s))
}

fn wrong(tried: &Tried<'_>) -> String {
    let name = tried.name;
    match (tried.kind, tried.login) {
        (Some(ProviderKind::Bedrock), true) => "AWS didn't accept the profile's credentials. If \
             it signs in with SSO, run aws sso login, then save it again."
            .into(),
        (Some(ProviderKind::Bedrock), false) => "AWS didn't accept those keys. Check the access \
             key ID and the secret access key were both copied whole; the secret is shown only \
             once, when the key is made."
            .into(),
        (Some(ProviderKind::VertexAnthropic), true) => "Google didn't accept your login. Run \
             gcloud auth application-default login again, then save it again."
            .into(),
        (Some(ProviderKind::VertexAnthropic), false) => "Google didn't accept that service \
             account key: it may have been deleted or turned off. Make a new key for the \
             service account, then paste the whole file."
            .into(),
        _ if tried.hint.is_empty() => {
            format!("{name} didn't accept that key. Check it was copied whole.")
        }
        _ => format!(
            "{name} didn't accept that key. Check it was copied whole, or make a new one: {}.",
            lowercase_first(tried.hint)
        ),
    }
}

/// Where the account gets credit.
fn top_up(tried: &Tried<'_>) -> String {
    match tried.kind {
        Some(ProviderKind::Anthropic) => "Add credit at console.anthropic.com under Billing".into(),
        Some(ProviderKind::OpenRouter) => "Add credit at openrouter.ai/credits".into(),
        Some(ProviderKind::Gemini) => {
            "Turn on billing for the key's project in Google AI Studio, or wait for its free \
             quota to come back"
                .into()
        }
        Some(ProviderKind::Grok) => "Add credit at console.x.ai".into(),
        Some(ProviderKind::Zai) => "Top up at z.ai, or buy a resource package".into(),
        Some(ProviderKind::Cerebras) => "Add credit at cloud.cerebras.ai".into(),
        Some(ProviderKind::Bedrock) => "Check the AWS account's billing".into(),
        Some(ProviderKind::VertexAnthropic) => {
            "Turn on billing for the project in the Google Cloud console".into()
        }
        _ => match tried.name {
            "Fish Audio" => "Add API credit at fish.audio".into(),
            "ElevenLabs" => "Upgrade the plan or add credits at elevenlabs.io".into(),
            "OpenAI" => "Add credit at platform.openai.com under Billing".into(),
            "Soniox" => "Add credit at console.soniox.com".into(),
            name => format!("Add credit to the {name} account"),
        },
    }
}

fn bedrock_refusal(lower: &str, tried: &Tried<'_>, region: &str, model: &str) -> Option<String> {
    if [
        "unrecognizedclientexception",
        "security token included in the request is invalid",
        "invalidsignatureexception",
        "signature we calculated does not match",
        "expiredtoken",
        "could not load credentials",
        "no credentials",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Some(wrong(tried));
    }
    if lower.contains("not authorized to perform") {
        return Some(
            "The keys work, but they aren't allowed to call Bedrock models. Give the IAM user \
             the bedrock:InvokeModel permission (the AmazonBedrockLimitedAccess policy has it), \
             then save it again."
                .into(),
        );
    }
    if lower.contains("access to the model") || lower.contains("accessdeniedexception") {
        return Some(format!(
            "The keys work, but this AWS account can't use {model} in {region} yet. Ask for it \
             under Model access in the Bedrock console for {region}, or choose the region where \
             it's turned on."
        ));
    }
    if [
        "model identifier is invalid",
        "isn't supported",
        "is not supported",
        "not available in",
        "on-demand throughput",
        "validationexception",
        "resourcenotfoundexception",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Some(format!(
            "Bedrock in {region} doesn't offer {model}. Choose a region that does, such as \
             us-east-1, us-west-2 or eu-west-1."
        ));
    }
    None
}

fn vertex_refusal(
    lower: &str,
    status: Option<u16>,
    tried: &Tried<'_>,
    model: &str,
) -> Option<String> {
    let project = tried.project.unwrap_or("the project");
    let location = tried.region.unwrap_or("global");
    if lower.contains("has not been used in project")
        || lower.contains("api is disabled")
        || lower.contains("service_disabled")
    {
        return Some(format!(
            "Turn on the Vertex AI API for {project} in the Google Cloud console, then save it \
             again."
        ));
    }
    if lower.contains("invalid_grant") || lower.contains("reauth") {
        return Some(wrong(tried));
    }
    match status {
        Some(403) => Some(format!(
            "Google knows the {}, but it can't call Vertex AI in {project}. Give it the Vertex \
             AI User role in IAM, and check Claude is turned on in the Model Garden.",
            if tried.login { "login" } else { "key" }
        )),
        Some(404) => Some(format!(
            "Vertex AI in {location} doesn't offer {model} to {project}. Try the global \
             location, or turn the model on in the Model Garden."
        )),
        _ => None,
    }
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// One call on a provider an agent thinks with, made the way the agent
/// makes it. Abstracted so tests never reach a provider.
#[async_trait]
pub(crate) trait TestCall: Send + Sync {
    /// A one-token answer from `model` on `kind`, with the `llm/accounts/*`
    /// record's key and extras. The error is the provider's; it's turned
    /// into words by [`refusal`] and never shown or logged as it is.
    async fn think(
        &self,
        kind: ProviderKind,
        api_key: &str,
        extras: &AccountExtras,
        model: &str,
    ) -> Result<(), String>;

    /// A word spoken by `provider` (`fish_audio`, `elevenlabs`, `openai`,
    /// `soniox`) with `key`.
    async fn speak(&self, provider: &str, key: &str) -> Result<(), String>;

    /// Whether a local server at `url` answers with its models.
    async fn reach(&self, server: Server, url: &str, token: Option<&str>) -> bool;
}

/// The test calls made for real.
pub(crate) struct Live;

#[async_trait]
impl TestCall for Live {
    async fn think(
        &self,
        kind: ProviderKind,
        api_key: &str,
        extras: &AccountExtras,
        model: &str,
    ) -> Result<(), String> {
        let provider = create_provider_with(kind, api_key, extras, Some(model), None, false)
            .map_err(|e| format!("{e:#}"))?;
        let hello = LlmMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "Hi".into() }],
        };
        let Err(error) = with_deadline(async {
            provider
                .create_message(None, &[hello], &[], Some(model), Some(1), false)
                .await
                .map(drop)
        })
        .await
        else {
            return Ok(());
        };
        if decisive(&error) {
            return Err(error);
        }
        // It may be the call, not the key: the key passes if the provider's
        // own check of it does.
        match with_deadline(provider.verify_credentials()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(error),
        }
    }

    async fn speak(&self, provider: &str, key: &str) -> Result<(), String> {
        let config = LlmSpeechConfig {
            provider: provider.into(),
            ..LlmSpeechConfig::default()
        };
        let speaker = provider_from_config(&config, Path::new(""), |_| KeySlot::fixed(key))
            .map_err(|e| format!("{e:#}"))?;
        with_deadline(async {
            speaker
                .synthesize(&SpeechRequest {
                    text: "Hi",
                    speed: None,
                    voice: None,
                })
                .await
                .map(drop)
        })
        .await
    }

    async fn reach(&self, server: Server, url: &str, token: Option<&str>) -> bool {
        local::installed(server, url, token, local::PROBE_TIMEOUT)
            .await
            .is_some()
    }
}

/// Answers every test call as told, and remembers what was asked: for tests
/// that must never reach a provider.
#[cfg(test)]
pub(crate) struct Answers {
    pub error: Option<String>,
    pub asked: parking_lot::Mutex<Vec<String>>,
}

#[cfg(test)]
impl Answers {
    pub(crate) fn passing() -> Self {
        Self {
            error: None,
            asked: Default::default(),
        }
    }

    pub(crate) fn failing(error: &str) -> Self {
        Self {
            error: Some(error.into()),
            asked: Default::default(),
        }
    }

    fn answer(&self, asked: String) -> Result<(), String> {
        self.asked.lock().push(asked);
        self.error.clone().map_or(Ok(()), Err)
    }
}

#[cfg(test)]
#[async_trait]
impl TestCall for Answers {
    async fn think(
        &self,
        kind: ProviderKind,
        _: &str,
        _: &AccountExtras,
        model: &str,
    ) -> Result<(), String> {
        self.answer(format!("{} {model}", kind.canonical_name()))
    }

    async fn speak(&self, provider: &str, _: &str) -> Result<(), String> {
        self.answer(format!("speak {provider}"))
    }

    async fn reach(&self, server: Server, url: &str, _: Option<&str>) -> bool {
        self.answer(format!("{server:?} {url}")).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tried(name: &'static str, kind: ProviderKind) -> Tried<'static> {
        Tried {
            name,
            kind: Some(kind),
            hint: "Create a key at console.example.com under API keys",
            model: Some("claude-sonnet-5-5"),
            ..Tried::default()
        }
    }

    #[test]
    fn a_wrong_key_says_so_and_where_to_make_a_new_one() {
        let anthropic = tried("Anthropic", ProviderKind::Anthropic);
        let said = refusal(
            &anthropic,
            r#"Anthropic API error (401 Unauthorized): {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        )
        .unwrap();
        assert_eq!(
            said,
            "Anthropic didn't accept that key. Check it was copied whole, or make a new one: \
             create a key at console.example.com under API keys."
        );
        // Gemini refuses a bad key with a 400.
        let gemini = tried("Google Gemini", ProviderKind::Gemini);
        let said = refusal(
            &gemini,
            "Gemini API error (400 Bad Request): API key not valid. Please pass a valid API key.",
        )
        .unwrap();
        assert!(
            said.starts_with("Google Gemini didn't accept that key"),
            "{said}"
        );
    }

    #[test]
    fn no_credit_says_where_to_add_it() {
        let anthropic = tried("Anthropic", ProviderKind::Anthropic);
        let said = refusal(
            &anthropic,
            "Anthropic API error (400 Bad Request): Your credit balance is too low to access the \
             Anthropic API.",
        )
        .unwrap();
        assert!(said.contains("no credit left"), "{said}");
        assert!(
            said.contains("console.anthropic.com under Billing"),
            "{said}"
        );
        let openrouter = tried("OpenRouter", ProviderKind::OpenRouter);
        let said = refusal(
            &openrouter,
            "OpenRouter API error (402 Payment Required): …",
        )
        .unwrap();
        assert!(said.contains("openrouter.ai/credits"), "{said}");
        // xAI says it with a 403, which is not a wrong key.
        let grok = tried("xAI Grok", ProviderKind::Grok);
        let said = refusal(
            &grok,
            "Grok API error (403 Forbidden): Your team doesn't have any credits or licenses yet.",
        )
        .unwrap();
        assert!(said.contains("no credit"), "{said}");
        let fish = Tried {
            name: "Fish Audio",
            ..Tried::default()
        };
        let said = refusal(&fish, "fish audio speech returned 402 Payment Required: …").unwrap();
        assert!(said.contains("fish.audio"), "{said}");
    }

    #[test]
    fn bedrock_says_which_part_was_wrong() {
        let keys = Tried {
            region: Some("eu-west-9"),
            model: Some("anthropic.claude-sonnet-5-5"),
            ..tried("Amazon Bedrock", ProviderKind::Bedrock)
        };
        let wrong = refusal(
            &keys,
            "Bedrock API error (403 Forbidden): UnrecognizedClientException: The security token \
             included in the request is invalid.",
        )
        .unwrap();
        assert!(wrong.contains("AWS didn't accept those keys"), "{wrong}");
        let unknown_region = refusal(
            &keys,
            "Bedrock HTTP request failed: error sending request for url \
             (https://bedrock-runtime.eu-west-9.amazonaws.com/)",
        )
        .unwrap();
        assert!(unknown_region.contains("in eu-west-9"), "{unknown_region}");
        assert!(unknown_region.contains("us-east-1"), "{unknown_region}");
        let no_model = refusal(
            &keys,
            "Bedrock API error (400 Bad Request): ValidationException: The provided model \
             identifier is invalid.",
        )
        .unwrap();
        assert!(
            no_model.starts_with("Bedrock in eu-west-9 doesn't offer"),
            "{no_model}"
        );
        let no_access = refusal(
            &keys,
            "Bedrock API error (403 Forbidden): AccessDeniedException: You don't have access to \
             the model with the specified model ID.",
        )
        .unwrap();
        assert!(no_access.contains("Model access"), "{no_access}");
        let profile = Tried {
            login: true,
            ..keys
        };
        let said = refusal(&profile, "Bedrock: could not load credentials from profile").unwrap();
        assert!(said.contains("aws sso login"), "{said}");
    }

    #[test]
    fn vertex_says_what_to_turn_on() {
        let vertex = Tried {
            project: Some("acme-ai"),
            region: Some("europe-west4"),
            ..tried("Google Vertex AI", ProviderKind::VertexAnthropic)
        };
        let off = refusal(
            &vertex,
            "Vertex AI 403 Forbidden (PERMISSION_DENIED): Vertex AI API has not been used in \
             project acme-ai before or it is disabled.",
        )
        .unwrap();
        assert!(
            off.contains("Turn on the Vertex AI API for acme-ai"),
            "{off}"
        );
        let missing = refusal(
            &vertex,
            "Vertex AI 404 Not Found (NOT_FOUND): Publisher model not found",
        )
        .unwrap();
        assert!(missing.contains("europe-west4"), "{missing}");
    }

    #[test]
    fn trouble_or_no_answer_says_to_try_again_and_a_limit_still_passes() {
        let anthropic = tried("Anthropic", ProviderKind::Anthropic);
        let busy = refusal(&anthropic, "Anthropic API error (529): overloaded_error").unwrap();
        assert!(busy.contains("having trouble"), "{busy}");
        let late = refusal(&anthropic, TIMED_OUT).unwrap();
        assert!(late.contains("didn't answer within"), "{late}");
        let offline = refusal(&anthropic, "HTTP request failed: error sending request").unwrap();
        assert!(offline.contains("couldn't reach Anthropic"), "{offline}");
        assert_eq!(
            refusal(
                &anthropic,
                "Anthropic API error (429 Too Many Requests): rate_limit_error"
            ),
            None
        );
        // An answer this client couldn't read is still an answer.
        assert_eq!(
            refusal(
                &anthropic,
                "Failed to parse Anthropic response: missing field"
            ),
            None
        );
    }

    #[test]
    fn a_refusal_of_the_call_rather_than_the_key_still_passes() {
        let grok = tried("xAI Grok", ProviderKind::Grok);
        assert_eq!(
            refusal(
                &grok,
                "Grok API error (400 Bad Request): max_tokens too small for a reasoning model"
            ),
            None
        );
        assert_eq!(
            refusal(&grok, "Grok API error (404 Not Found): model not found"),
            None
        );
        assert!(!decisive(
            "OpenRouter API error (400): upstream provider returned an error"
        ));
        assert!(!decisive("Anthropic API error (529): overloaded_error"));
        for told in [
            "Anthropic API error (401 Unauthorized): invalid x-api-key",
            "Anthropic API error (400): Your credit balance is too low",
            "OpenRouter API error (429): rate limited",
            "HTTP request failed: error sending request",
            TIMED_OUT,
        ] {
            assert!(decisive(told), "{told}");
        }
    }

    #[test]
    fn a_refusal_never_repeats_what_the_provider_said() {
        // Made at run time, so no scanner takes the fixture for a real key.
        let key = ["sk", "proj", "AbCdEf0123456789xyz"].join("-");
        let key = key.as_str();
        let openrouter = tried("OpenRouter", ProviderKind::OpenRouter);
        for error in [
            format!("OpenRouter API error (401): Incorrect API key provided: {key}"),
            format!("OpenRouter API error (403): teapot {key}"),
            format!("OpenRouter API error (402): {key} has no credit"),
        ] {
            let said = refusal(&openrouter, &error).unwrap();
            assert!(!said.contains(key) && !said.contains("sk-proj"), "{said}");
            assert!(!said.contains("teapot"), "{said}");
        }
    }

    #[test]
    fn statuses_read_in_each_clients_form() {
        assert_eq!(
            status_of("anthropic api error (401 unauthorized): x"),
            Some(401)
        );
        assert_eq!(
            status_of("fish audio speech returned 402 payment required"),
            Some(402)
        );
        assert_eq!(status_of("gemini verify: http 403 forbidden: x"), Some(403));
        assert_eq!(status_of("request 8a0b-4015 failed"), None);
        assert_eq!(status_of("returned 200 ok"), None);
    }
}
