//! Integration credentials, resolved through the layered vault.
//!
//! An integration that needs an API key names it as a [`VaultCredential`] and
//! reads it with [`Caller::resolve`] each time it is used, for the person and
//! agent the use is for. The install config and the environment are only a
//! last resort for installs that predate the vault entry. Reads go through
//! the same [`LayeredUserVault`] and precedence the skill adapters use
//! ([`resolve_at`]): global per user, agent per user, agent, global. A
//! credential whose scope is shared skips the per-user tiers ([`shared_get`]).
//! Vault reads are in-memory, so a key rotated in the vault is used on the
//! next call without a restart.
//!
//! The same descriptor is the credential's entry in the vault editor's
//! catalog, so the dropdown and the reader never disagree on the path.

use std::sync::Arc;

use workforce_agent::llm::provider::{chain_credential_fallback, ProviderKind};
use workforce_agent::security::ssrf::is_private_ip;
use workforce_common::secret::{ApiKeySource, KeySlot};
use workforce_common::settings::{
    LlmImageGenerationConfig, LlmSpeechConfig, LlmVideoGenerationConfig, SecretStore, VaultSource,
};
use workforce_skills::adapters::auth::resolve_at;

use crate::skill_loader::{CatalogEntry, CredManaged, CredScope};
use crate::user_config::LayeredUserVault;
use crate::vault::Vault;

/// A credential an integration reads from the vault, and how the vault editor
/// offers it.
pub struct VaultCredential {
    pub path: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub scope: CredScope,
    /// The environment variable read when no vault tier holds a key.
    pub env: Option<&'static str>,
    /// Read from the global vault alone: the hub builds one client with it
    /// for every agent.
    pub hub_wide: bool,
}

impl VaultCredential {
    /// The value of [`Self::env`], when it names a variable that is set.
    pub fn from_env(&self) -> Option<String> {
        self.env
            .and_then(|var| std::env::var(var).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

pub const JEV_API_KEY: VaultCredential = VaultCredential {
    path: "decision/jev/api_key",
    group: "decision",
    label: "Jev (TypeSafe) — API key",
    description: "TypeSafe API key for the Jev decision model (plain value). Per user so each \
                  person's decisions use their own account, or agent-specific/global for a key \
                  everyone shares. Without one at any level, Jev is skipped for that person.",
    scope: CredScope::Either,
    env: None,
    hub_wide: false,
};

pub const KEV_API_KEY: VaultCredential = VaultCredential {
    path: "decision/kev/api_key",
    group: "decision",
    label: "Kev — service key",
    description: "The key the Kev server was started with (KEV_API_KEY), shared by every user \
                  of the agent or install",
    scope: CredScope::Shared,
    env: None,
    hub_wide: false,
};

pub const EMBEDDINGS_OPENAI_API_KEY: VaultCredential = VaultCredential {
    path: "embeddings/openai/api_key",
    group: "embeddings",
    label: "OpenAI-compatible embeddings — API key",
    description: "Key for the [embeddings] provider = \"openai\" endpoint. Hub-wide: set it in \
                  the global vault",
    scope: CredScope::Shared,
    env: Some("EMBEDDINGS_API_KEY"),
    hub_wide: true,
};

pub const MEMORY_EMBEDDINGS_API_KEY: VaultCredential = VaultCredential {
    path: "embeddings/memory/api_key",
    group: "embeddings",
    label: "SQLite memory embeddings — API key",
    description: "Key for the [memory] embedding_provider = \"openai\" endpoint of the SQLite \
                  memory store. Hub-wide: set it in the global vault",
    scope: CredScope::Shared,
    env: Some("OPENAI_API_KEY"),
    hub_wide: true,
};

pub const BRAVE_API_KEY: VaultCredential = VaultCredential {
    path: workforce_agent::tools::web::BRAVE_KEY_VAULT_PATH,
    group: "tools",
    label: "Brave Search — API key",
    description: "Brave Search API key for the web.search tool. Per user so each person \
                  searches on their own account, or agent-specific/global for a key everyone \
                  shares. Without one at any level, web.search is unavailable to that person.",
    scope: CredScope::Either,
    env: Some("BRAVE_API_KEY"),
    hub_wide: false,
};

pub const SUMMARISER_API_KEY: VaultCredential = VaultCredential {
    path: "knowledge/summariser/api_key",
    group: "knowledge",
    label: "Knowledge summariser — API key",
    description: "Key for the provider [knowledge.summariser] names. Hub-wide: set it in the \
                  global vault",
    scope: CredScope::Shared,
    env: None,
    hub_wide: true,
};

const CATALOG: &[&VaultCredential] = &[
    &JEV_API_KEY,
    &KEV_API_KEY,
    &EMBEDDINGS_OPENAI_API_KEY,
    &MEMORY_EMBEDDINGS_API_KEY,
    &BRAVE_API_KEY,
    &SUMMARISER_API_KEY,
];

/// The kinds of media work an agent hands to a provider its `[llm.*]` config
/// names, and one credential for each provider that takes a key people
/// bring, at `<group>/<provider>/api_key`. The provider is in the path so a
/// key is only ever sent to the provider that issued it, whichever one an
/// agent's config routes the work to.
macro_rules! modalities {
    ($($modality:ident = $group:literal, $title:literal, $purpose:literal:
        [$(($provider:literal, $vendor:literal, $label:literal)),+];)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Modality {
            $($modality),+
        }

        impl Modality {
            const fn group(self) -> &'static str {
                match self {
                    $(Self::$modality => $group),+
                }
            }

            const fn title(self) -> &'static str {
                match self {
                    $(Self::$modality => $title),+
                }
            }
        }

        const PROVIDER_KEYS: &[VaultCredential] = &[$($(VaultCredential {
            path: concat!($group, "/", $provider, "/api_key"),
            group: $group,
            label: $label,
            description: concat!(
                $vendor, " API key (plain value) that pays for ", $purpose, " on agents that \
                route it to ", $vendor, ". Per user so each person uses their own account, or \
                agent-specific/global for a key everyone shares."
            ),
            scope: CredScope::Either,
            env: None,
            hub_wide: false,
        },)+)+];
    };
}

modalities! {
    Vision = "vision", "Image description", "describing images": [
        ("anthropic", "Anthropic", "Image description — Anthropic API key"),
        ("openrouter", "OpenRouter", "Image description — OpenRouter API key"),
        ("gemini", "Gemini", "Image description — Gemini API key")
    ];
    Transcription = "transcription", "Transcription", "transcribing audio and video": [
        ("openai", "OpenAI", "Transcription — OpenAI API key"),
        ("groq", "Groq", "Transcription — Groq API key"),
        ("gemini", "Gemini", "Transcription — Gemini API key"),
        ("openrouter", "OpenRouter", "Transcription — OpenRouter API key")
    ];
    Image = "image", "Image generation", "generating images": [
        ("gemini", "Gemini", "Image generation — Gemini API key"),
        ("openrouter", "OpenRouter", "Image generation — OpenRouter API key")
    ];
    Video = "video", "Video generation", "generating video": [
        ("gemini", "Gemini", "Video generation — Gemini API key")
    ];
    Speech = "speech", "Speech", "speaking text aloud": [
        ("fish_audio", "Fish Audio", "Fish Audio — API key"),
        ("elevenlabs", "ElevenLabs", "ElevenLabs — API key"),
        ("openai", "OpenAI", "OpenAI speech — API key"),
        ("soniox", "Soniox", "Soniox — API key")
    ];
}

/// Every catalogued credential.
pub fn catalog() -> impl Iterator<Item = &'static VaultCredential> {
    CATALOG.iter().copied().chain(PROVIDER_KEYS)
}

/// The catalogued credential at `path`, if any.
pub fn catalogued(path: &str) -> Option<&'static VaultCredential> {
    catalog().find(|c| c.path == path)
}

/// The credential `modality` reads when it is routed to `provider` (its
/// canonical name), when that provider takes a key people bring.
pub fn for_provider(modality: Modality, provider: &str) -> Option<&'static VaultCredential> {
    catalogued(&format!("{}/{provider}/api_key", modality.group()))
}

/// Every integration credential, as vault editor catalog entries.
pub fn catalog_entries() -> impl Iterator<Item = CatalogEntry> {
    catalog().map(|c| CatalogEntry {
        id: c.path.to_string(),
        skill: None,
        group: c.group.to_string(),
        label: c.label.to_string(),
        template: c.path.to_string(),
        variables: Vec::new(),
        source: "builtin",
        addable: true,
        scope: c.scope.as_str(),
        managed_by: CredManaged::Manual.as_str(),
        per_user_allowed: c.scope.allows_per_user(),
        required: false,
        description: c.description.to_string(),
        multiline: false,
    })
}

/// Whether a member may save `path` in their own per-user tiers: it is a
/// catalogued credential people bring their own key to.
pub fn is_personal(path: &str) -> bool {
    catalogued(path).is_some_and(|c| c.scope.allows_per_user())
}

/// How a key gets to `path`, said to a person or to an agent that must not
/// collect it: a personal credential is the person's to add, any other an
/// admin's, in the global vault alone when the hub reads no other.
pub fn how_to_add(path: &str) -> String {
    let how = if is_personal(path) {
        format!(
            "A person adds their own in My Settings (vault path {path}), or an admin sets \
             one for the agent or the install"
        )
    } else if catalogued(path).is_some_and(|c| c.hub_wide) {
        format!("An admin sets it in the global vault at {path}")
    } else {
        format!("An admin sets it in the agent's or the global vault at {path}")
    };
    format!("{how}. Do not ask for the key in chat or try to store it yourself.")
}

/// `credential` read on every call for whoever is in scope there, then
/// `last_resort` (an install's config or environment key).
pub fn source(
    credential: &'static VaultCredential,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    last_resort: Option<String>,
) -> ApiKeySource {
    source_then(credential, agent_vault, global_vault, move || {
        last_resort.clone()
    })
}

/// [`source`] whose last resort is itself read on every call.
pub fn source_then(
    credential: &'static VaultCredential,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    last_resort: impl Fn() -> Option<String> + Send + Sync + 'static,
) -> ApiKeySource {
    let agent_vault = agent_vault.filter(|_| !credential.hub_wide);
    Arc::new(move || {
        Caller::in_scope(agent_vault.clone(), global_vault.clone())
            .credential(credential)
            .or_else(&last_resort)
    })
}

/// What a credential's reader falls back on when no vault level holds one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastResort {
    /// A key in the install or agent config.
    Config(&'static str),
    /// A variable in the hub's environment.
    Environment(&'static str),
    /// An `llm/accounts/*` vault entry for the provider.
    Account,
    /// The provider's own credential chain (an AWS role, Google ADC).
    Chain,
}

type Read = Box<dyn Fn() -> Option<String> + Send + Sync>;

/// Where an integration's key came from before the vault held it: the
/// install's config, its environment and its LLM accounts, in the order that
/// integration always tried them. Read on every call, after the vault.
#[derive(Default)]
pub struct Legacy(Vec<(LastResort, Read)>);

impl Legacy {
    pub fn config(mut self, field: &'static str, value: Option<String>) -> Self {
        self.0
            .push((LastResort::Config(field), Box::new(move || value.clone())));
        self
    }

    pub fn env(mut self, vars: &[&'static str]) -> Self {
        self.0.extend(vars.iter().map(|&var| {
            let read: Read = Box::new(move || std::env::var(var).ok());
            (LastResort::Environment(var), read)
        }));
        self
    }

    /// The API key of the vault LLM account for `provider`, agent then global.
    pub fn account(
        mut self,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
        provider: &str,
    ) -> Self {
        let provider = provider.to_string();
        self.0.push((
            LastResort::Account,
            Box::new(move || {
                crate::vault::find_account_api_key_layered(
                    agent_vault.as_deref(),
                    global_vault.as_deref(),
                    &provider,
                )
            }),
        ));
        self
    }

    /// The sentinel that sends `kind` to its own credential chain.
    pub fn chain(mut self, kind: ProviderKind) -> Self {
        self.0.push((
            LastResort::Chain,
            Box::new(move || chain_credential_fallback(kind)),
        ));
        self
    }

    /// The first step that is set, and its key. A step set to nothing ends
    /// the chain with no key, as it always did: the steps behind it are not
    /// tried.
    pub fn held(&self) -> Option<(LastResort, String)> {
        self.0
            .iter()
            .find_map(|(from, read)| read().map(|key| (*from, key.trim().to_string())))
            .filter(|(_, key)| !key.is_empty())
    }

    pub fn key(&self) -> Option<String> {
        self.held().map(|(_, key)| key)
    }

    #[cfg(test)]
    fn order(&self) -> Vec<LastResort> {
        self.0.iter().map(|(from, _)| *from).collect()
    }

    /// The chain with the hub's environment left out, for a test that must
    /// not depend on the variables of whoever runs it.
    #[cfg(test)]
    fn without_env(mut self) -> Self {
        self.0
            .retain(|(from, _)| !matches!(from, LastResort::Environment(_)));
        self
    }
}

/// Every credential the agent behind `service` reads, with what its reader
/// falls back on: the ones every agent reads, the summariser's when summaries
/// run on a provider that takes a key and, for each modality, the one for
/// the provider the agent's config routes it to.
pub fn readers(service: &crate::serve::AgentService) -> Vec<(&'static VaultCredential, Legacy)> {
    use workforce_agent::attachments::{Describer, Transcriber};
    let (agent, global) = (&service.vault, &service.global_vault);
    let knowledge = &service.config.knowledge;
    let summarises = knowledge.enabled
        && knowledge.generate_summaries
        && crate::knowledge::service::summariser_takes_key(&knowledge.summariser);
    let fixed = CATALOG
        .iter()
        .copied()
        .filter(|credential| credential.path != SUMMARISER_API_KEY.path || summarises)
        .map(|credential| {
            let legacy = match credential.path {
                path if path == MEMORY_EMBEDDINGS_API_KEY.path => Legacy::default().config(
                    "[memory] embedding_api_key",
                    service.config.memory.embedding_api_key.clone(),
                ),
                path if path == SUMMARISER_API_KEY.path => {
                    crate::knowledge::service::summariser_legacy(&knowledge.summariser)
                }
                _ => Legacy::default(),
            };
            (credential, legacy.env(credential.env.as_slice()))
        });
    let vision = match &service.describer {
        Describer::Ready(_, cfg) => crate::vision::route(cfg, &service.vision_tiers)
            .ok()
            .and_then(|(kind, _)| {
                for_provider(Modality::Vision, kind.canonical_name())
                    .map(|credential| (credential, crate::vision::legacy(cfg, kind)))
            }),
        Describer::Unconfigured | Describer::Misconfigured(_) => None,
    };
    let transcription = match &service.transcriber {
        Transcriber::Ready(_, cfg) => {
            crate::transcription::route(cfg, &service.transcription_tiers)
                .ok()
                .and_then(|(backend, provider, _)| {
                    for_provider(Modality::Transcription, &provider).map(|credential| {
                        let legacy = crate::transcription::legacy(
                            cfg,
                            backend,
                            &provider,
                            agent.clone(),
                            global.clone(),
                        );
                        (credential, legacy)
                    })
                })
        }
        Transcriber::Unconfigured | Transcriber::Misconfigured(_) => None,
    };
    let image = service.image_config.read().as_ref().and_then(|cfg| {
        let kind = workforce_agent::llm::image::routed_provider(cfg).ok()?;
        for_provider(Modality::Image, kind.canonical_name()).map(|credential| {
            (
                credential,
                image_legacy(cfg, kind, agent.clone(), global.clone()),
            )
        })
    });
    let video = service.video_config.read().as_ref().and_then(|cfg| {
        for_provider(Modality::Video, VIDEO_PROVIDER)
            .map(|credential| (credential, video_legacy(cfg, agent.clone(), global.clone())))
    });
    let speech = service.speech_config.read().as_ref().and_then(|cfg| {
        let provider = workforce_agent::llm::speech::canonical_speech_provider(&cfg.provider)?;
        for_provider(Modality::Speech, provider).map(|credential| {
            (
                credential,
                speech_legacy(cfg, provider, agent.clone(), global.clone()),
            )
        })
    });
    fixed
        .chain(
            [vision, transcription, image, video, speech]
                .into_iter()
                .flatten(),
        )
        .collect()
}

/// The key `modality` sends `provider`: that provider's credential for
/// whoever is in scope at each call, then `legacy`. A provider with no
/// credential here (a self-hosted endpoint, a credential chain) has only
/// `legacy`.
pub fn slot(
    modality: Modality,
    provider: &str,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
    legacy: Legacy,
) -> KeySlot {
    let what = modality.title();
    match for_provider(modality, provider) {
        Some(credential) => KeySlot::new(
            source_then(credential, agent_vault, global_vault, move || legacy.key()),
            unavailable(what, credential),
        ),
        None => KeySlot::new(
            Arc::new(move || legacy.key()),
            format!("{what} is not available: the install has no API key for {provider}."),
        ),
    }
}

/// What is said when `what` is asked for and `credential` holds no key for
/// the caller.
pub fn unavailable(what: &str, credential: &VaultCredential) -> String {
    let whom = match credential.scope.allows_per_user() {
        true => " to you",
        false => "",
    };
    format!(
        "{what} is not available{whom}: no API key for it is set. {}",
        how_to_add(credential.path)
    )
}

/// What `[llm.image]` read its key from before the vault credential:
/// `api_key`, `IMAGE_API_KEY`, the provider's LLM account, its credential
/// chain, its environment variable.
pub fn image_legacy(
    cfg: &LlmImageGenerationConfig,
    kind: ProviderKind,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> Legacy {
    Legacy::default()
        .config("[llm.image] api_key", cfg.api_key.clone())
        .env(&["IMAGE_API_KEY"])
        .account(agent_vault, global_vault, kind.canonical_name())
        .chain(kind)
        .env(provider_env(kind))
}

/// What `[llm.video]` read its key from before the vault credential.
pub fn video_legacy(
    cfg: &LlmVideoGenerationConfig,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> Legacy {
    Legacy::default()
        .config("[llm.video] api_key", cfg.api_key.clone())
        .env(&["VIDEO_API_KEY"])
        .account(agent_vault, global_vault, VIDEO_PROVIDER)
        .env(&["GEMINI_API_KEY"])
}

/// What `[llm.speech]` read its shared key from besides the vault path.
pub fn speech_legacy(
    cfg: &LlmSpeechConfig,
    provider: &'static str,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> Legacy {
    let env: &[&'static str] = match provider {
        "openai" => &["OPENAI_API_KEY"],
        "elevenlabs" => &["ELEVENLABS_API_KEY"],
        "fish_audio" => &["FISH_AUDIO_API_KEY"],
        "soniox" => &["SONIOX_API_KEY"],
        _ => &[],
    };
    Legacy::default()
        .config("[llm.speech] api_key", cfg.api_key.clone())
        .env(&["SPEECH_API_KEY"])
        .account(agent_vault, global_vault, provider)
        .env(env)
}

/// The provider Veo's key belongs to.
pub const VIDEO_PROVIDER: &str = "gemini";

/// The environment variables a bearer-key provider's key is read from; none
/// for a provider that takes no such key.
pub fn provider_env(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::Anthropic => &["ANTHROPIC_API_KEY"],
        ProviderKind::OpenRouter => &["OPENROUTER_API_KEY"],
        ProviderKind::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ProviderKind::Grok => &["XAI_API_KEY"],
        ProviderKind::Cerebras => &["CEREBRAS_API_KEY"],
        ProviderKind::Zai => &["ZAI_API_KEY"],
        ProviderKind::Ollama
        | ProviderKind::Vllm
        | ProviderKind::Bedrock
        | ProviderKind::VertexAnthropic
        | ProviderKind::ChatGpt
        | ProviderKind::ClaudeCode
        | ProviderKind::Codex => &[],
    }
}

/// The key source `generate_image` hands the provider its config routes to.
pub fn image_keys(
    cfg: Option<&LlmImageGenerationConfig>,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> impl Fn(ProviderKind) -> KeySlot {
    let (cfg, agent_vault, global_vault) = (
        cfg.cloned().unwrap_or_default(),
        agent_vault.clone(),
        global_vault.clone(),
    );
    move |kind| {
        slot(
            Modality::Image,
            kind.canonical_name(),
            agent_vault.clone(),
            global_vault.clone(),
            image_legacy(&cfg, kind, agent_vault.clone(), global_vault.clone()),
        )
    }
}

/// The key source `generate_video` hands Veo.
pub fn video_keys(
    cfg: Option<&LlmVideoGenerationConfig>,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> KeySlot {
    slot(
        Modality::Video,
        VIDEO_PROVIDER,
        agent_vault.clone(),
        global_vault.clone(),
        video_legacy(
            &cfg.cloned().unwrap_or_default(),
            agent_vault.clone(),
            global_vault.clone(),
        ),
    )
}

/// The key source `generate_speech` hands the provider its config names.
pub fn speech_keys(
    cfg: Option<&LlmSpeechConfig>,
    agent_vault: &Option<Arc<Vault>>,
    global_vault: &Option<Arc<Vault>>,
) -> impl Fn(&'static str) -> KeySlot {
    let (cfg, agent_vault, global_vault) = (
        cfg.cloned().unwrap_or_default(),
        agent_vault.clone(),
        global_vault.clone(),
    );
    move |provider| {
        slot(
            Modality::Speech,
            provider,
            agent_vault.clone(),
            global_vault.clone(),
            speech_legacy(&cfg, provider, agent_vault.clone(), global_vault.clone()),
        )
    }
}

/// The Brave Search key for whoever calls `web.search` on an agent with
/// these vaults, then `BRAVE_API_KEY`.
pub fn brave_source(
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
) -> ApiKeySource {
    source(
        &BRAVE_API_KEY,
        agent_vault,
        global_vault,
        BRAVE_API_KEY.from_env(),
    )
}

/// Whether `url` is an address on a private network: a private, loopback or
/// link-local IP, a single-label host (`kev`, `localhost`) or a name in a
/// zone that only resolves internally. Judged from the URL alone, so every
/// replica agrees. Anything else is a public hosted endpoint, which never
/// takes a request without a key.
pub fn is_internal(url: &url::Url) -> bool {
    const INTERNAL_ZONES: &[&str] = &[
        ".local",
        ".internal",
        ".localhost",
        ".lan",
        ".svc",
        ".home.arpa",
    ];
    match url.host() {
        Some(url::Host::Ipv4(ip)) => is_private_ip(&ip.into()),
        Some(url::Host::Ipv6(ip)) => is_private_ip(&ip.into()),
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            !name.contains('.') || INTERNAL_ZONES.iter().any(|zone| name.ends_with(zone))
        }
        None => true,
    }
}

/// Whom a credential is resolved for: a person (none for system work, which
/// sees only the shared tiers), the agent's own vault and the hub's global one.
#[derive(Clone, Default)]
pub struct Caller {
    person: Option<String>,
    agent_vault: Option<Arc<Vault>>,
    global_vault: Option<Arc<Vault>>,
}

impl Caller {
    pub fn new(
        person: Option<String>,
        agent_vault: Option<Arc<Vault>>,
        global_vault: Option<Arc<Vault>>,
    ) -> Self {
        Self {
            person: person.filter(|p| !p.is_empty()),
            agent_vault,
            global_vault,
        }
    }

    /// For `person` working with `service`'s agent.
    pub fn for_service(service: &crate::serve::AgentService, person: Option<String>) -> Self {
        Self::new(person, service.vault.clone(), service.global_vault.clone())
    }

    /// For whoever `CURRENT_USER_ID` names here.
    pub fn in_scope(agent_vault: Option<Arc<Vault>>, global_vault: Option<Arc<Vault>>) -> Self {
        Self::new(
            crate::user_config::current_identity_id(),
            agent_vault,
            global_vault,
        )
    }

    pub fn person(&self) -> Option<&str> {
        self.person.as_deref()
    }

    /// The tier that holds `path` for this caller and its value, honouring
    /// `scope`: trimmed, and `None` when the winning tier's is empty.
    fn held(&self, path: &str, scope: CredScope) -> Option<(VaultSource, String)> {
        let (store, global_only) = match (&self.agent_vault, &self.global_vault) {
            (Some(agent), global) => (
                LayeredUserVault::new(Arc::clone(agent), global.clone()),
                false,
            ),
            (None, Some(global)) => (LayeredUserVault::new(Arc::clone(global), None), true),
            (None, None) => return None,
        };
        // Which value wins is `shared_get`'s and `resolve_at`'s to say; the
        // tiers are walked only to name the one that holds it.
        let read = || {
            let tiers = store.inspect(path);
            match scope {
                CredScope::Shared => tiers.into_iter().find(|(tier, _)| !tier.is_per_user()),
                CredScope::PerUser | CredScope::Either => {
                    let won = resolve_at(&store, path)?;
                    let tier = tiers
                        .into_iter()
                        .find(|(tier, value)| {
                            tier.is_per_user() == won.per_user && *value == won.value
                        })
                        .map_or(VaultSource::Unknown, |(tier, _)| tier);
                    Some((tier, won.value))
                }
            }
        };
        let (tier, value) = workforce_agent::runner::CURRENT_USER_ID
            .sync_scope(self.person.clone().unwrap_or_default(), read)?;
        let tier = match (global_only, tier) {
            (true, VaultSource::AgentPerUser) => VaultSource::GlobalPerUser,
            (true, VaultSource::AgentSpecific) => VaultSource::Global,
            (_, tier) => tier,
        };
        Some((tier, value.trim().to_string())).filter(|(_, value)| !value.is_empty())
    }

    /// The value at `path` for this caller, honouring `scope`: trimmed, and
    /// `None` when no tier holds a non-empty one.
    pub fn resolve(&self, path: &str, scope: CredScope) -> Option<String> {
        self.held(path, scope).map(|(_, value)| value)
    }

    /// The vault tier [`Self::resolve`] reads `path` from, if any does.
    pub fn level(&self, path: &str, scope: CredScope) -> Option<VaultSource> {
        self.held(path, scope).map(|(tier, _)| tier)
    }

    /// [`Self::resolve`] for a catalogued credential.
    pub fn credential(&self, credential: &VaultCredential) -> Option<String> {
        self.resolve(credential.path, credential.scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::runner::CURRENT_USER_ID;

    const PATH: &str = "decision/jev/api_key";

    fn vault(dir: &std::path::Path, name: &str) -> Arc<Vault> {
        Arc::new(Vault::open(&dir.join(name), "test-key").unwrap())
    }

    fn caller(person: Option<&str>, agent: &Arc<Vault>, global: &Arc<Vault>) -> Caller {
        Caller::new(
            person.map(str::to_string),
            Some(Arc::clone(agent)),
            Some(Arc::clone(global)),
        )
    }

    #[test]
    fn each_level_resolves_and_the_most_specific_wins() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let alice = caller(Some("alice"), &agent, &global);
        assert_eq!(alice.resolve(PATH, CredScope::Either), None);

        global.set(PATH, "global").unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("global")
        );
        agent.set(PATH, "agent").unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("agent")
        );
        agent
            .set(&format!("users/alice/{PATH}"), "agent-alice")
            .unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("agent-alice")
        );
        global
            .set(&format!("users/alice/{PATH}"), " global-alice \n")
            .unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("global-alice"),
            "the skill adapters' precedence: global per user first"
        );
    }

    #[test]
    fn the_level_names_the_tier_the_value_is_read_from() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let alice = caller(Some("alice"), &agent, &global);
        let level = |scope| alice.level(PATH, scope);
        assert_eq!(level(CredScope::Either), None);

        global.set(PATH, "global").unwrap();
        assert_eq!(level(CredScope::Either), Some(VaultSource::Global));
        agent.set(PATH, "agent").unwrap();
        assert_eq!(level(CredScope::Either), Some(VaultSource::AgentSpecific));
        agent
            .set(&format!("users/alice/{PATH}"), "agent-alice")
            .unwrap();
        assert_eq!(level(CredScope::Either), Some(VaultSource::AgentPerUser));
        global
            .set(&format!("users/alice/{PATH}"), "global-alice")
            .unwrap();
        assert_eq!(level(CredScope::Either), Some(VaultSource::GlobalPerUser));
        assert_eq!(level(CredScope::Shared), Some(VaultSource::AgentSpecific));
        assert_eq!(
            caller(Some("bob"), &agent, &global).level(PATH, CredScope::Either),
            Some(VaultSource::AgentSpecific),
            "another person's key is not a level for this one"
        );
        assert_eq!(
            caller(None, &agent, &global).level(PATH, CredScope::Either),
            Some(VaultSource::AgentSpecific)
        );

        global.set(&format!("users/alice/{PATH}"), "  ").unwrap();
        assert_eq!(
            (
                level(CredScope::Either),
                alice.resolve(PATH, CredScope::Either)
            ),
            (None, None),
            "an empty winner is no key, exactly as the reader sees it"
        );

        let global_only = Caller::new(Some("alice".into()), None, Some(Arc::clone(&global)));
        assert_eq!(
            global_only.level(PATH, CredScope::Shared),
            Some(VaultSource::Global)
        );
    }

    #[test]
    fn a_person_sees_only_their_own_per_user_key() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        global
            .set(&format!("users/alice/{PATH}"), "alice-key")
            .unwrap();
        agent.set(&format!("users/bob/{PATH}"), "bob-key").unwrap();
        let resolve = |p| caller(Some(p), &agent, &global).resolve(PATH, CredScope::Either);
        assert_eq!(resolve("alice").as_deref(), Some("alice-key"));
        assert_eq!(resolve("bob").as_deref(), Some("bob-key"));
        assert_eq!(resolve("carol"), None);
    }

    #[test]
    fn system_work_and_shared_credentials_see_only_the_shared_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        agent
            .set(&format!("users/alice/{PATH}"), "alice-key")
            .unwrap();
        let system = caller(None, &agent, &global);
        let outer =
            CURRENT_USER_ID.sync_scope("alice".into(), || system.resolve(PATH, CredScope::Either));
        assert_eq!(outer, None, "an identity outside the call never leaks in");
        assert_eq!(
            caller(Some("alice"), &agent, &global).resolve(PATH, CredScope::Shared),
            None,
            "a shared credential ignores per-user values"
        );
        global.set(PATH, "org").unwrap();
        assert_eq!(
            system.resolve(PATH, CredScope::Either).as_deref(),
            Some("org")
        );
    }

    #[test]
    fn any_vault_combination_resolves_and_none_resolves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let global = vault(dir.path(), "g.enc");
        global
            .set(&format!("users/alice/{PATH}"), "alice-key")
            .unwrap();
        let global_only = Caller::new(Some("alice".into()), None, Some(Arc::clone(&global)));
        assert_eq!(
            global_only.resolve(PATH, CredScope::Either).as_deref(),
            Some("alice-key")
        );
        let agent_only = Caller::new(Some("alice".into()), Some(global), None);
        assert_eq!(
            agent_only.resolve(PATH, CredScope::Either).as_deref(),
            Some("alice-key")
        );
        assert_eq!(
            Caller::new(Some("alice".into()), None, None).resolve(PATH, CredScope::Either),
            None
        );
    }

    #[test]
    fn a_rotated_key_is_read_on_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let alice = caller(Some("alice"), &agent, &global);
        global.set(&format!("users/alice/{PATH}"), "old").unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("old")
        );
        global.set(&format!("users/alice/{PATH}"), "new").unwrap();
        assert_eq!(
            alice.resolve(PATH, CredScope::Either).as_deref(),
            Some("new")
        );
        global.delete(&format!("users/alice/{PATH}")).unwrap();
        assert_eq!(alice.resolve(PATH, CredScope::Either), None);
    }

    #[test]
    fn an_address_is_internal_only_when_it_cannot_be_a_public_endpoint() {
        let internal = |url: &str| is_internal(&url::Url::parse(url).unwrap());
        for url in [
            "http://kev:8009",
            "http://localhost:8009",
            "http://127.0.0.1:8009",
            "http://10.20.0.5:8009",
            "http://192.168.1.4",
            "http://100.64.0.7",
            "http://[::1]:8009",
            "http://[fd00::1]:8009",
            "http://kev.prod.example.local:8009",
            "http://kev.default.svc.cluster.local",
            "http://kev.default.svc",
            "http://KEV.Internal.",
            "http://box.lan",
            "http://nas.home.arpa",
        ] {
            assert!(internal(url), "{url}");
        }
        for url in [
            "https://api.typesafe.ai",
            "https://kev.example.com",
            "http://8.8.8.8:8009",
            "https://[2606:4700::1111]",
            "https://notlocal.example",
            "https://internal.example.com",
        ] {
            assert!(!internal(url), "{url}");
        }
    }

    #[test]
    fn a_source_reads_for_whoever_is_in_scope_at_each_call_then_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let key = source(
            &BRAVE_API_KEY,
            Some(Arc::clone(&agent)),
            Some(Arc::clone(&global)),
            Some("env".into()),
        );
        let as_person = |p: &str| CURRENT_USER_ID.sync_scope(p.to_string(), || key());
        global
            .set(&format!("users/alice/{}", BRAVE_API_KEY.path), "alice")
            .unwrap();
        assert_eq!(as_person("alice").as_deref(), Some("alice"));
        assert_eq!(as_person("bob").as_deref(), Some("env"));
        assert_eq!(key().as_deref(), Some("env"));
        agent.set(BRAVE_API_KEY.path, "shared").unwrap();
        assert_eq!(as_person("bob").as_deref(), Some("shared"));
        assert_eq!(as_person("alice").as_deref(), Some("alice"));

        let none = source(&BRAVE_API_KEY, None, Some(global), None);
        assert_eq!(CURRENT_USER_ID.sync_scope("bob".into(), || none()), None);
    }

    const SECRETS: [&str; 6] = [
        "sk-config",
        "sk-account",
        "sk-global",
        "sk-agent",
        "sk-agent-alice",
        "sk-global-alice",
    ];

    fn transcription_slot(agent: &Arc<Vault>, global: &Arc<Vault>) -> KeySlot {
        slot(
            Modality::Transcription,
            "groq",
            Some(Arc::clone(agent)),
            Some(Arc::clone(global)),
            Legacy::default()
                .config("[llm.transcription] api_key", Some("sk-config".into()))
                .account(Some(Arc::clone(agent)), Some(Arc::clone(global)), "groq"),
        )
    }

    #[test]
    fn a_modality_key_is_read_at_each_level_for_the_person_in_scope_then_the_legacy_chain() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let path = "transcription/groq/api_key";
        let key = transcription_slot(&agent, &global);
        let as_person = |p: &str| CURRENT_USER_ID.sync_scope(p.to_string(), || key.get());

        global
            .set(
                "llm/accounts/stale",
                r#"{"provider":"groq","api_key":"sk-account"}"#,
            )
            .unwrap();
        assert_eq!(
            as_person("alice").as_deref(),
            Ok("sk-config"),
            "the install's own key, in the order it always read it, when no level holds one"
        );
        global.set(path, "sk-global").unwrap();
        assert_eq!(as_person("alice").as_deref(), Ok("sk-global"));
        agent.set(path, "sk-agent").unwrap();
        assert_eq!(as_person("alice").as_deref(), Ok("sk-agent"));
        agent
            .set(&format!("users/alice/{path}"), "sk-agent-alice")
            .unwrap();
        assert_eq!(as_person("alice").as_deref(), Ok("sk-agent-alice"));
        global
            .set(&format!("users/alice/{path}"), "sk-global-alice")
            .unwrap();
        assert_eq!(as_person("alice").as_deref(), Ok("sk-global-alice"));
        assert_eq!(
            as_person("bob").as_deref(),
            Ok("sk-agent"),
            "another person's key is never bob's"
        );
        assert_eq!(key.get().as_deref(), Ok("sk-agent"), "nor system work's");

        global
            .set(&format!("users/alice/{path}"), "sk-rotated")
            .unwrap();
        assert_eq!(
            as_person("alice").as_deref(),
            Ok("sk-rotated"),
            "a rotated key is used on the next call"
        );

        global.delete(&format!("users/alice/{path}")).unwrap();
        agent.delete(&format!("users/alice/{path}")).unwrap();
        assert_eq!(
            as_person("alice").as_deref(),
            Ok("sk-agent"),
            "a deleted key is gone on the next call"
        );
    }

    #[test]
    fn a_person_with_no_key_is_told_how_to_add_one_and_is_never_served_on_anothers() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let key = slot(
            Modality::Transcription,
            "groq",
            Some(Arc::clone(&agent)),
            Some(Arc::clone(&global)),
            Legacy::default(),
        );
        for (vault, secret) in [(&global, SECRETS[5]), (&agent, SECRETS[4])] {
            vault
                .set("users/alice/transcription/groq/api_key", secret)
                .unwrap();
        }
        let refused = CURRENT_USER_ID
            .sync_scope("bob".to_string(), || key.get())
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "Transcription is not available to you: no API key for it is set. A person adds \
             their own in My Settings (vault path transcription/groq/api_key), or an admin sets \
             one for the agent or the install. Do not ask for the key in chat or try to store it \
             yourself."
        );
        assert!(SECRETS.iter().all(|secret| !refused.contains(secret)));
        assert_eq!(format!("{key:?}"), "KeySlot(***)");

        let self_hosted = slot(
            Modality::Transcription,
            "vllm",
            Some(agent),
            Some(global),
            Legacy::default(),
        );
        assert_eq!(self_hosted.peek(), None);
        assert_eq!(
            self_hosted.get().unwrap_err().to_string(),
            "Transcription is not available: the install has no API key for vllm."
        );
    }

    #[test]
    fn the_legacy_chain_reads_its_steps_in_order_and_names_the_one_that_holds_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        let chain = |config: Option<&str>| {
            Legacy::default()
                .config("[llm.x] api_key", config.map(str::to_string))
                .env(&["DW599_UNSET_TEST_KEY"])
                .account(Some(Arc::clone(&agent)), Some(Arc::clone(&global)), "groq")
                .chain(ProviderKind::Bedrock)
        };
        assert_eq!(
            chain(Some(" from-config ")).held(),
            Some((LastResort::Config("[llm.x] api_key"), "from-config".into()))
        );
        assert_eq!(
            chain(Some("  ")).held(),
            None,
            "a step set to nothing ends the chain, as it did before the vault"
        );
        global
            .set(
                "llm/accounts/groq",
                r#"{"provider":"groq","api_key":"from-global-account"}"#,
            )
            .unwrap();
        assert_eq!(
            chain(None).held(),
            Some((LastResort::Account, "from-global-account".into()))
        );
        agent
            .set(
                "llm/accounts/mine",
                r#"{"provider":"groq","api_key":"from-agent-account"}"#,
            )
            .unwrap();
        assert_eq!(chain(None).key().as_deref(), Some("from-agent-account"));
        assert_eq!(Legacy::default().held(), None);
        assert_eq!(
            image_legacy(
                &LlmImageGenerationConfig::default(),
                ProviderKind::Bedrock,
                None,
                None
            )
            .held()
            .map(|(from, _)| from),
            Some(LastResort::Chain),
            "Bedrock with no stored account runs on its own credential chain, as before"
        );
    }

    #[test]
    fn every_reader_falls_back_on_what_it_read_before_the_vault_in_the_same_order() {
        use workforce_common::settings::{
            KnowledgeSummariserConfig, LlmTranscriptionConfig, LlmVisionConfig,
        };
        use LastResort::{Account, Chain, Config, Environment as Env};

        let vision = |kind| crate::vision::legacy(&LlmVisionConfig::default(), kind).order();
        let vision_config = Config("[llm.vision] api_key");
        for (kind, provider_env) in [
            (ProviderKind::Bedrock, vec!["AWS_ACCESS_KEY_ID"]),
            (
                ProviderKind::VertexAnthropic,
                vec!["GOOGLE_APPLICATION_CREDENTIALS"],
            ),
            (ProviderKind::Vllm, vec!["VLLM_API_KEY"]),
            (ProviderKind::Ollama, vec![]),
            (ProviderKind::ChatGpt, vec![]),
            (ProviderKind::Anthropic, vec!["ANTHROPIC_API_KEY"]),
            (ProviderKind::OpenRouter, vec!["OPENROUTER_API_KEY"]),
            (
                ProviderKind::Gemini,
                vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            ),
            (ProviderKind::Grok, vec!["XAI_API_KEY"]),
        ] {
            let expected: Vec<LastResort> = [vision_config, Env("VISION_API_KEY")]
                .into_iter()
                .chain(provider_env.into_iter().map(Env))
                .collect();
            assert_eq!(vision(kind), expected, "{kind:?}");
        }

        let tiers =
            workforce_agent::llm::transcription_tiers::TranscriptionTierRegistry::from_defaults()
                .unwrap();
        for (spec, provider_env) in [
            ("openai:standard", vec!["OPENAI_API_KEY"]),
            ("groq:cheap", vec!["GROQ_API_KEY"]),
            ("gemini:standard", vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            ("openrouter:google:cheap", vec!["OPENROUTER_API_KEY"]),
        ] {
            let config: LlmTranscriptionConfig =
                toml::from_str(&format!("model = {spec:?}")).unwrap();
            let (backend, provider, _) = crate::transcription::route(&config, &tiers).unwrap();
            let expected: Vec<LastResort> = [
                Config("[llm.transcription] api_key"),
                Env("TRANSCRIPTION_API_KEY"),
            ]
            .into_iter()
            .chain(provider_env.into_iter().map(Env))
            .chain([Account])
            .collect();
            assert_eq!(
                crate::transcription::legacy(&config, backend, &provider, None, None).order(),
                expected,
                "{spec}"
            );
        }

        let image =
            |kind| image_legacy(&LlmImageGenerationConfig::default(), kind, None, None).order();
        let image_head = [
            Config("[llm.image] api_key"),
            Env("IMAGE_API_KEY"),
            Account,
            Chain,
        ];
        assert_eq!(image(ProviderKind::Bedrock), image_head);
        assert_eq!(
            image(ProviderKind::Gemini),
            [
                image_head.as_slice(),
                &[Env("GEMINI_API_KEY"), Env("GOOGLE_API_KEY")]
            ]
            .concat()
        );
        assert_eq!(
            image(ProviderKind::OpenRouter),
            [image_head.as_slice(), &[Env("OPENROUTER_API_KEY")]].concat()
        );
        assert_eq!(
            image(ProviderKind::Vllm),
            image_head,
            "a self-hosted endpoint's address is no longer its bearer token"
        );

        assert_eq!(
            video_legacy(&LlmVideoGenerationConfig::default(), None, None).order(),
            [
                Config("[llm.video] api_key"),
                Env("VIDEO_API_KEY"),
                Account,
                Env("GEMINI_API_KEY")
            ]
        );
        for (provider, var) in [
            ("openai", "OPENAI_API_KEY"),
            ("elevenlabs", "ELEVENLABS_API_KEY"),
            ("fish_audio", "FISH_AUDIO_API_KEY"),
            ("soniox", "SONIOX_API_KEY"),
        ] {
            assert_eq!(
                speech_legacy(&LlmSpeechConfig::default(), provider, None, None).order(),
                [
                    Config("[llm.speech] api_key"),
                    Env("SPEECH_API_KEY"),
                    Account,
                    Env(var)
                ]
            );
        }

        let summariser = |provider: &str| {
            crate::knowledge::service::summariser_legacy(&KnowledgeSummariserConfig {
                provider: provider.into(),
                model: "some-model".into(),
                ..Default::default()
            })
            .order()
        };
        let summariser_config = Config("[knowledge.summariser] api_key");
        assert_eq!(summariser("bedrock"), [summariser_config]);
        assert_eq!(summariser("ollama"), [summariser_config]);
        assert_eq!(
            summariser("openrouter"),
            [summariser_config, Env("OPENROUTER_API_KEY")]
        );
    }

    #[test]
    fn a_credential_chain_provider_never_reads_a_key_from_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (vault(dir.path(), "a.enc"), vault(dir.path(), "g.enc"));
        for group in ["vision", "image", "transcription"] {
            for vault in [&agent, &global] {
                for path in [
                    format!("{group}/bedrock/api_key"),
                    format!("users/alice/{group}/bedrock/api_key"),
                ] {
                    vault.set(&path, "planted").unwrap();
                }
            }
        }
        let read = |modality, legacy: Legacy| {
            let key = slot(
                modality,
                "bedrock",
                Some(Arc::clone(&agent)),
                Some(Arc::clone(&global)),
                legacy,
            );
            CURRENT_USER_ID.sync_scope("alice".to_string(), || key.peek())
        };
        assert_eq!(read(Modality::Vision, Legacy::default()), None);
        assert_eq!(
            read(
                Modality::Image,
                image_legacy(
                    &LlmImageGenerationConfig::default(),
                    ProviderKind::Bedrock,
                    Some(Arc::clone(&agent)),
                    Some(Arc::clone(&global)),
                )
                .without_env()
            )
            .as_deref(),
            Some(workforce_agent::llm::bedrock_credentials::CHAIN_SENTINEL),
            "Bedrock image generation runs on the role, as before"
        );
    }

    #[test]
    fn generation_keys_are_the_callers_own_then_shared_then_the_configs() {
        let dir = tempfile::tempdir().unwrap();
        let (agent, global) = (
            Some(vault(dir.path(), "a.enc")),
            Some(vault(dir.path(), "g.enc")),
        );
        let (agent_vault, global_vault) = (agent.clone().unwrap(), global.clone().unwrap());
        let configured = |key: &str| Some(key.to_string());
        let image = image_keys(
            Some(&LlmImageGenerationConfig {
                api_key: configured("image-config"),
                ..Default::default()
            }),
            &agent,
            &global,
        )(ProviderKind::Gemini);
        let video = video_keys(
            Some(&LlmVideoGenerationConfig {
                api_key: configured("video-config"),
                ..Default::default()
            }),
            &agent,
            &global,
        );
        let speech = speech_keys(
            Some(&LlmSpeechConfig {
                api_key: configured("speech-config"),
                ..Default::default()
            }),
            &agent,
            &global,
        )("fish_audio");
        let as_person =
            |slot: &KeySlot, p: &str| CURRENT_USER_ID.sync_scope(p.to_string(), || slot.get());

        for (slot, path, config) in [
            (&image, "image/gemini/api_key", "image-config"),
            (&video, "video/gemini/api_key", "video-config"),
            (&speech, "speech/fish_audio/api_key", "speech-config"),
        ] {
            assert_eq!(as_person(slot, "alice").as_deref(), Ok(config), "{path}");
            agent_vault.set(path, "shared").unwrap();
            agent_vault
                .set(&format!("users/alice/{path}"), "alice-agent")
                .unwrap();
            global_vault
                .set(&format!("users/alice/{path}"), "alice-global")
                .unwrap();
            assert_eq!(
                as_person(slot, "alice").as_deref(),
                Ok("alice-global"),
                "{path}: the key saved in My Settings wins"
            );
            assert_eq!(as_person(slot, "bob").as_deref(), Ok("shared"), "{path}");
            assert_eq!(slot.get().as_deref(), Ok("shared"), "{path}: system work");
        }
    }

    #[test]
    fn every_modality_provider_is_catalogued_once_for_people_to_bring_their_own_key() {
        let paths: Vec<&str> = catalog().map(|c| c.path).collect();
        let unique: std::collections::HashSet<&str> = paths.iter().copied().collect();
        assert_eq!(paths.len(), unique.len(), "{paths:?}");
        for (modality, provider, path, label) in [
            (
                Modality::Vision,
                "openrouter",
                "vision/openrouter/api_key",
                "Image description — OpenRouter API key",
            ),
            (
                Modality::Transcription,
                "groq",
                "transcription/groq/api_key",
                "Transcription — Groq API key",
            ),
            (
                Modality::Image,
                "gemini",
                "image/gemini/api_key",
                "Image generation — Gemini API key",
            ),
            (
                Modality::Video,
                "gemini",
                "video/gemini/api_key",
                "Video generation — Gemini API key",
            ),
            (
                Modality::Speech,
                "fish_audio",
                "speech/fish_audio/api_key",
                "Fish Audio — API key",
            ),
        ] {
            let credential = for_provider(modality, provider).unwrap();
            assert_eq!((credential.path, credential.label), (path, label));
            assert!(is_personal(path) && !credential.hub_wide, "{path}");
        }
        for (modality, provider) in [
            (Modality::Vision, "bedrock"),
            (Modality::Vision, "chatgpt"),
            (Modality::Transcription, "vllm"),
            (Modality::Image, "bedrock"),
            (Modality::Video, "openrouter"),
            (Modality::Speech, "builtin"),
        ] {
            assert!(for_provider(modality, provider).is_none(), "{provider}");
        }
        let summariser = catalogued("knowledge/summariser/api_key").unwrap();
        assert!(summariser.hub_wide && !is_personal(summariser.path));
        assert_eq!(
            how_to_add(summariser.path),
            "An admin sets it in the global vault at knowledge/summariser/api_key. Do not ask \
             for the key in chat or try to store it yourself."
        );
    }

    #[test]
    fn the_catalog_offers_jev_at_every_level_and_the_service_keys_shared() {
        let entries: Vec<CatalogEntry> = catalog_entries().collect();
        let entry = |path: &str| entries.iter().find(|e| e.id == path).unwrap();
        let jev = entry(JEV_API_KEY.path);
        assert_eq!(jev.label, "Jev (TypeSafe) — API key");
        assert_eq!((jev.scope, jev.per_user_allowed), ("either", true));
        let brave = entry(BRAVE_API_KEY.path);
        assert_eq!((brave.scope, brave.per_user_allowed), ("either", true));
        assert!(is_personal(JEV_API_KEY.path) && is_personal(BRAVE_API_KEY.path));
        assert!(!is_personal(KEV_API_KEY.path) && !is_personal("decision/jev/other"));
        for shared in [
            KEV_API_KEY.path,
            EMBEDDINGS_OPENAI_API_KEY.path,
            MEMORY_EMBEDDINGS_API_KEY.path,
        ] {
            assert_eq!(
                (entry(shared).scope, entry(shared).per_user_allowed),
                ("shared", false)
            );
        }
        assert!(catalogued("decision/jev/api_key").is_some());
        assert!(catalogued("decision/other/api_key").is_none());
    }
}
