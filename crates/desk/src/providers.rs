//! The providers a Desk agent can think with: each one's plan, key or local
//! server, how Desk connects it, the models it offers and the efforts it
//! honours. The tables are Desk's; whether a provider is connected, the
//! efforts the hub honours and a local server's installed models come from
//! the hub through [`Accounts`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What a provider runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    /// A subscription the person signs in to.
    Plan,
    /// An API key the person brings.
    Key,
    /// A model server on this machine or network.
    Local,
}

/// How Desk connects a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectKind {
    /// Desk's own one-click sign-in.
    Login,
    /// The fields below, sent to `POST /api/desk/providers/{id}/connect`.
    Fields,
    /// A server that is found where it runs; its fields say where.
    Probe,
}

/// One thing the person types to connect a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Field {
    pub name: &'static str,
    pub label: &'static str,
    pub secret: bool,
    pub required: bool,
    /// Long enough to want a text area (a pasted key file).
    pub multiline: bool,
    pub placeholder: &'static str,
    /// What the field starts with, when that is what most people want
    /// (the `default` AWS profile).
    #[serde(skip_serializing_if = "str::is_empty")]
    pub initial: &'static str,
}

impl Field {
    const fn secret(name: &'static str, label: &'static str) -> Self {
        Self {
            name,
            label,
            secret: true,
            required: true,
            multiline: false,
            placeholder: "",
            initial: "",
        }
    }

    const fn optional(self, placeholder: &'static str) -> Self {
        Self {
            required: false,
            placeholder,
            ..self
        }
    }

    const fn plain(name: &'static str, label: &'static str, placeholder: &'static str) -> Self {
        Self {
            name,
            label,
            secret: false,
            required: true,
            multiline: false,
            placeholder,
            initial: "",
        }
    }

    const fn starting_at(self, initial: &'static str) -> Self {
        Self { initial, ..self }
    }
}

/// One of the ways a provider connects, when it has more than one: the
/// person picks one, and only its fields are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Way {
    pub id: &'static str,
    pub label: &'static str,
    pub fields: &'static [Field],
    pub hint: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Connect {
    pub kind: ConnectKind,
    /// The fields, or with [`Self::ways`] the first way's (what a Desk that
    /// knows no ways shows).
    pub fields: &'static [Field],
    pub hint: &'static str,
    /// The ways to choose between, when there is more than one.
    #[serde(skip_serializing_if = "<[Way]>::is_empty")]
    pub ways: &'static [Way],
}

/// A model a provider offers, as the picker names it. An agent runs on it
/// with `[llm] provider = provider` and `default_model = id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub description: String,
    pub default: bool,
    /// The `[llm] provider` the model is reached through: the provider's id,
    /// with the model family for those the hub routes by family
    /// (`bedrock:anthropic`, `openrouter:openai`).
    pub provider: String,
    /// The efforts this model takes, when they are not its provider's; an
    /// empty list is no effort control.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub efforts: Option<Efforts>,
    /// How the plan words the model's faster tier ("2x speed, increased
    /// usage"), when it has one an agent or session can ask for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<String>,
}

/// Effort levels, lowest first, and the one to start at (none without any).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Efforts {
    pub efforts: Vec<&'static str>,
    pub default_effort: Option<&'static str>,
}

impl Efforts {
    fn of(efforts: Vec<&'static str>) -> Self {
        let default_effort = efforts
            .iter()
            .copied()
            .find(|e| *e == DEFAULT_EFFORT)
            .or_else(|| efforts.first().copied());
        Self {
            efforts,
            default_effort,
        }
    }
}

/// The `[llm] provider` value that reaches `model` on the provider `id`.
pub fn route(id: &str, model: &str) -> String {
    let family = match id {
        "bedrock" => model.split('.').next(),
        "openrouter" => model.split('/').next().map(|lab| match lab {
            "moonshotai" => "kimi",
            "z-ai" => "glm",
            lab => lab,
        }),
        "cerebras" => Some(match model {
            m if m.starts_with("qwen") => "qwen",
            _ => "gpt",
        }),
        "vertex-anthropic" => return "vertex:anthropic".into(),
        _ => None,
    };
    match family {
        Some(family) => format!("{id}:{family}"),
        None => id.into(),
    }
}

/// A curated model: its id as the hub routes it, its name and one line.
type Curated = (&'static str, &'static str, &'static str);

/// One provider in the picker.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    pub name: &'static str,
    pub group: Group,
    pub description: &'static str,
    pub connect: Connect,
    /// Most capable first; the one marked `true` is the default.
    models: &'static [(Curated, bool)],
}

impl Spec {
    /// The curated models' ids, most capable first.
    pub fn model_ids(&self) -> impl Iterator<Item = &'static str> {
        self.models.iter().map(|((id, _, _), _)| *id)
    }

    /// Whether the models are the server's own, listed live.
    pub fn lists_installed(&self) -> bool {
        self.group == Group::Local
    }

    fn curated(&self) -> Vec<Model> {
        self.models
            .iter()
            .map(|&((id, name, description), default)| Model {
                id: id.into(),
                name: name.into(),
                description: description.into(),
                default,
                provider: route(self.id, id),
                efforts: None,
                fast: None,
            })
            .collect()
    }
}

const API_KEY: Field = Field::secret("api_key", "API key");

const BEDROCK_REGION: Field = Field::plain("region", "Region", "us-east-1").optional("us-east-1");
const BEDROCK_KEYS: &[Field] = &[
    Field::secret("access_key_id", "Access key ID"),
    Field::secret("secret_access_key", "Secret access key"),
    BEDROCK_REGION,
];
const BEDROCK_KEYS_HINT: &str =
    "An IAM user's access key allowed to invoke Bedrock models in the region";

const fn keyed(hint: &'static str) -> Connect {
    Connect {
        kind: ConnectKind::Fields,
        fields: &[API_KEY],
        hint,
        ways: &[],
    }
}

const fn login(hint: &'static str) -> Connect {
    Connect {
        kind: ConnectKind::Login,
        fields: &[],
        hint,
        ways: &[],
    }
}

const SONNET: &str = "Fast and strong — the everyday choice";
const OPUS: &str = "Deep reasoning for complex, multi-step work";
const FABLE: &str = "Anthropic's most capable model, for the hardest work";
const HAIKU: &str = "The quickest and lightest, for simple tasks";

/// Every provider Desk offers, in the order the picker shows them: plans,
/// then keys, then local servers. Codex is left out: it runs on a remote
/// worker, never on a local hub.
pub const PROVIDERS: &[Spec] = &[
    Spec {
        id: "claude-code",
        name: "Claude plan",
        group: Group::Plan,
        description: "Your Pro or Max subscription",
        connect: login("Sign in with your Claude account"),
        models: &[
            (("fable", "Fable 5.2", FABLE), false),
            (("opus", "Opus 5.5", OPUS), false),
            (("sonnet", "Sonnet 5.5", SONNET), true),
            (("haiku", "Haiku 5.5", HAIKU), false),
        ],
    },
    Spec {
        id: "chatgpt",
        name: "ChatGPT plan",
        group: Group::Plan,
        description: "Your Plus, Pro or Business subscription",
        connect: login("Sign in with your ChatGPT account"),
        models: &[
            (
                (
                    "gpt-6-astra",
                    "GPT-6 Astra",
                    "OpenAI's most capable model, for the hardest work",
                ),
                false,
            ),
            (
                (
                    "gpt-6.1-sol",
                    "GPT-6.1 Sol",
                    "Strong and quick — the everyday choice",
                ),
                true,
            ),
            (
                (
                    "gpt-6-sol",
                    "GPT-6 Sol",
                    "The previous Sol, steady and capable",
                ),
                false,
            ),
            (("gpt-6-luna", "GPT-6 Luna", HAIKU), false),
            (
                (
                    "gpt-5.6-sol",
                    "GPT-5.6 Sol",
                    "Older generation workhorse model",
                ),
                false,
            ),
            (
                (
                    "gpt-5.6-terra",
                    "GPT-5.6 Terra",
                    "Older balanced model for straightforward work",
                ),
                false,
            ),
            (
                (
                    "gpt-5.6-luna",
                    "GPT-5.6 Luna",
                    "Older fast and efficient model",
                ),
                false,
            ),
        ],
    },
    Spec {
        id: "anthropic",
        name: "Anthropic",
        group: Group::Key,
        description: "Claude on an Anthropic API key, billed per use",
        connect: keyed("Create a key at console.anthropic.com under API keys"),
        models: &[
            (("claude-fable-5-2", "Claude Fable 5.2", FABLE), false),
            (("claude-fable-5-1", "Claude Fable 5.1", FABLE), false),
            (("claude-opus-5-5", "Claude Opus 5.5", OPUS), false),
            (("claude-sonnet-5-5", "Claude Sonnet 5.5", SONNET), true),
            (("claude-haiku-5-5", "Claude Haiku 5.5", HAIKU), false),
        ],
    },
    Spec {
        id: "bedrock",
        name: "Amazon Bedrock",
        group: Group::Key,
        description: "Claude and open models on your AWS account",
        connect: Connect {
            kind: ConnectKind::Fields,
            fields: BEDROCK_KEYS,
            hint: BEDROCK_KEYS_HINT,
            ways: &[
                Way {
                    id: "keys",
                    label: "Keys",
                    fields: BEDROCK_KEYS,
                    hint: BEDROCK_KEYS_HINT,
                },
                Way {
                    id: "profile",
                    label: "AWS profile",
                    fields: &[
                        Field::plain("profile", "Profile", "default").starting_at("default"),
                        BEDROCK_REGION,
                    ],
                    hint: "A profile in ~/.aws/config on this computer: keys, SSO after \
                           aws sso login, or a credential process. Desk refreshes it as it \
                           expires",
                },
            ],
        },
        models: &[
            (
                (
                    "anthropic.claude-fable-5-2",
                    "Claude Fable 5.2",
                    "Anthropic's most capable model; served through global routing",
                ),
                false,
            ),
            (
                (
                    "anthropic.claude-fable-5-1",
                    "Claude Fable 5.1",
                    "The previous Fable; served through global routing",
                ),
                false,
            ),
            (
                ("anthropic.claude-opus-5-5", "Claude Opus 5.5", OPUS),
                false,
            ),
            (
                ("anthropic.claude-sonnet-5-5", "Claude Sonnet 5.5", SONNET),
                true,
            ),
            (
                ("anthropic.claude-haiku-5-5", "Claude Haiku 5.5", HAIKU),
                false,
            ),
            (
                (
                    "qwen.qwen3-coder-480b-a35b-v1:0",
                    "Qwen3 Coder 480B",
                    "Qwen's open coding model",
                ),
                false,
            ),
            (
                ("deepseek.v3.2", "DeepSeek V3.2", "DeepSeek's open model"),
                false,
            ),
            (("zai.glm-5", "GLM 5", "Z.ai's open model"), false),
        ],
    },
    Spec {
        id: "vertex-anthropic",
        name: "Google Vertex AI",
        group: Group::Key,
        description: "Claude on your Google Cloud project",
        connect: Connect {
            kind: ConnectKind::Fields,
            fields: &[
                Field {
                    multiline: true,
                    required: false,
                    placeholder:
                        "Empty to use your Google login, or { \"type\": \"service_account\", … }",
                    ..Field::secret("service_account_json", "Service account key (JSON)")
                },
                Field::plain("location", "Location", "global").optional("global"),
                Field::plain("project", "Project ID", "")
                    .optional("Needed with your Google login; a key names its own"),
            ],
            hint: "Leave the key empty to use your Google login on this Mac and give your \
                   Project ID; you may need to run gcloud auth application-default login \
                   first. Or paste a service account key with the Vertex AI User role, from \
                   IAM in the Google Cloud console.",
            ways: &[],
        },
        models: &[
            (("claude-opus-5-5", "Claude Opus 5.5", OPUS), false),
            (("claude-sonnet-5-5", "Claude Sonnet 5.5", SONNET), true),
            (
                ("claude-haiku-4-5@20251001", "Claude Haiku 4.5", HAIKU),
                false,
            ),
        ],
    },
    Spec {
        id: "gemini",
        name: "Google Gemini",
        group: Group::Key,
        description: "Gemini on a Google AI Studio key",
        connect: keyed("Create a key at aistudio.google.com"),
        models: &[
            (
                (
                    "gemini-3.1-pro-preview",
                    "Gemini 3.1 Pro",
                    "Google's most capable model",
                ),
                false,
            ),
            (
                (
                    "gemini-3.8-flash",
                    "Gemini 3.8 Flash",
                    "Fast and capable — the everyday choice",
                ),
                true,
            ),
            (
                ("gemini-3.5-flash-lite", "Gemini 3.5 Flash-Lite", HAIKU),
                false,
            ),
        ],
    },
    Spec {
        id: "openrouter",
        name: "OpenRouter",
        group: Group::Key,
        description: "Models from many labs on one key",
        connect: keyed("Create a key at openrouter.ai/keys"),
        models: &[
            (
                ("anthropic/claude-opus-5.5", "Claude Opus 5.5", OPUS),
                false,
            ),
            (
                (
                    "openai/gpt-6.1-sol",
                    "GPT-6.1 Sol",
                    "OpenAI's everyday model",
                ),
                false,
            ),
            (
                (
                    "google/gemini-3.1-pro-preview",
                    "Gemini 3.1 Pro",
                    "Google's most capable model",
                ),
                false,
            ),
            (
                ("anthropic/claude-sonnet-5.5", "Claude Sonnet 5.5", SONNET),
                true,
            ),
            (
                (
                    "deepseek/deepseek-v4-pro",
                    "DeepSeek V4 Pro",
                    "DeepSeek's strongest open model",
                ),
                false,
            ),
            (
                ("moonshotai/kimi-k3", "Kimi K3", "Moonshot's open model"),
                false,
            ),
            (("z-ai/glm-5.3", "GLM 5.3", "Z.ai's open model"), false),
            (
                (
                    "deepseek/deepseek-v4.1-flash",
                    "DeepSeek V4.1 Flash",
                    "Very cheap, for simple tasks",
                ),
                false,
            ),
        ],
    },
    Spec {
        id: "grok",
        name: "xAI Grok",
        group: Group::Key,
        description: "Grok on an xAI API key",
        connect: keyed("Create a key at console.x.ai"),
        models: &[
            (("grok-4.7", "Grok 4.7", "xAI's most capable model"), true),
            (
                ("grok-4.3", "Grok 4.3", "Cheaper, with a 1M-token context"),
                false,
            ),
        ],
    },
    Spec {
        id: "zai",
        name: "Z.ai",
        group: Group::Key,
        description: "GLM on a Z.ai API key",
        connect: keyed("Create a key at z.ai under API keys"),
        models: &[
            (("glm-5.3", "GLM 5.3", "Z.ai's most capable model"), true),
            (("glm-5.3-flash", "GLM 5.3 Flash", HAIKU), false),
        ],
    },
    Spec {
        id: "cerebras",
        name: "Cerebras",
        group: Group::Key,
        description: "Open models at very high speed",
        connect: keyed("Create a key at cloud.cerebras.ai"),
        models: &[
            (
                (
                    "qwen-3.8-27b",
                    "Qwen 3.8 27B",
                    "Qwen's open model, very fast",
                ),
                false,
            ),
            (
                (
                    "gpt-oss-120b",
                    "GPT-OSS 120B",
                    "OpenAI's open model, very fast",
                ),
                true,
            ),
        ],
    },
    Spec {
        id: "ollama",
        name: "Ollama",
        group: Group::Local,
        description: "Models running in Ollama on this Mac or your network",
        connect: Connect {
            kind: ConnectKind::Probe,
            fields: &[Field::plain("url", "Server URL", "http://localhost:11434")],
            hint: "Install Ollama from ollama.com and pull a model; Desk finds it on its \
                   usual port",
            ways: &[],
        },
        models: &[],
    },
    Spec {
        id: "vllm",
        name: "vLLM",
        group: Group::Local,
        description: "A vLLM server you run",
        connect: Connect {
            kind: ConnectKind::Fields,
            fields: &[
                Field::plain("url", "Server URL", "http://localhost:8000"),
                API_KEY.optional("Only if the server asks for one"),
            ],
            hint: "The address of an OpenAI-compatible vLLM server",
            ways: &[],
        },
        models: &[],
    },
];

/// The effort a provider starts at, when it takes one.
pub const DEFAULT_EFFORT: &str = "medium";

/// The provider with `id`.
pub fn spec(id: &str) -> Option<&'static Spec> {
    PROVIDERS.iter().find(|spec| spec.id == id)
}

/// What the hub knows about a provider right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Standing {
    /// Its credentials are where the hub reads them (a local server answers,
    /// or has an address).
    pub connected: bool,
    /// The models a local server has installed, when it answered.
    pub installed: Vec<String>,
    /// The models a plan lists for its login now, when it could be asked:
    /// what Desk offers in place of (or beside) its own table, so a model
    /// released since Desk was shows up.
    pub listed: Option<Vec<Listed>>,
}

/// A model a plan lists, as its service words it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    /// The model an alias runs now (`sonnet` → `claude-sonnet-5-5`).
    pub resolves_to: Option<String>,
    pub name: String,
    pub description: String,
    /// The effort levels the plan lists for it; only those the hub honours
    /// on the provider are offered.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    /// How the plan words its faster tier, when it has one.
    pub fast: Option<String>,
}

/// The fields a connect request carries, by name.
pub type Fields = BTreeMap<String, String>;

/// Why a provider could not be connected or disconnected. A message never
/// carries what was typed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Internal(String),
}

/// The hub's side of the providers: where their credentials live, and what
/// its runtime honours.
#[async_trait]
pub trait Accounts: Send + Sync {
    /// Whether `provider` is connected, and a local server's models.
    async fn standing(&self, provider: &Spec) -> Standing;
    /// The effort levels the hub honours on `provider`, lowest first; none
    /// when it has no effort control.
    fn efforts(&self, provider: &str) -> Vec<&'static str>;
    /// The effort levels the hub honours on `model` through `provider`:
    /// fewer than the provider's for a model it can send no effort to.
    fn model_efforts(&self, provider: &str, _model: &str) -> Vec<&'static str> {
        self.efforts(provider)
    }
    /// Stores `fields` (already checked against the provider's) where the
    /// hub reads `provider`'s credentials.
    async fn connect(&self, provider: &Spec, fields: &Fields) -> Result<(), AccountError>;
    /// Removes every credential the hub would read for `provider`.
    async fn disconnect(&self, provider: &Spec) -> Result<(), AccountError>;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    id: &'static str,
    name: &'static str,
    group: Group,
    description: &'static str,
    connected: bool,
    connect: Connect,
    models: Vec<Model>,
    efforts: Vec<&'static str>,
    default_effort: Option<&'static str>,
}

impl Entry {
    pub(crate) async fn of(spec: &'static Spec, accounts: &dyn Accounts) -> Self {
        let standing = accounts.standing(spec).await;
        let honoured = accounts.efforts(spec.id);
        let mut models = match (spec.lists_installed(), standing.listed) {
            (true, _) => installed(spec.id, standing.installed),
            (false, Some(listed)) if !listed.is_empty() => with_listed(spec, listed, &honoured),
            (false, _) => spec.curated(),
        };
        let provider_efforts = Efforts::of(honoured);
        for model in &mut models {
            let own = match model.efforts.take() {
                Some(listed) => listed,
                None => Efforts::of(accounts.model_efforts(spec.id, &model.id)),
            };
            if own != provider_efforts {
                model.efforts = Some(own);
            }
        }
        let Efforts {
            efforts,
            default_effort,
        } = provider_efforts;
        Self {
            id: spec.id,
            name: spec.name,
            group: spec.group,
            description: spec.description,
            connected: standing.connected,
            connect: spec.connect,
            models,
            efforts,
            default_effort,
        }
    }
}

/// `spec`'s models with what its plan or key lists now. A ChatGPT plan's list is
/// the one it serves: Desk's table only words the models it knows, and its
/// default stays the default while listed. A Claude plan's aliases always
/// run (each names the newest of its line), so they stay first, named as
/// the plan names them, and every model the plan lists that no alias
/// already offers follows them by its full name.
fn with_listed(spec: &Spec, listed: Vec<Listed>, honoured: &[&'static str]) -> Vec<Model> {
    let curated = spec.curated();
    let entry = |l: &Listed, default: bool| Model {
        id: l.id.clone(),
        name: l.name.clone(),
        description: l.description.clone(),
        default,
        provider: route(spec.id, &l.id),
        efforts: Some(listed_efforts(l, honoured)),
        fast: l.fast.clone(),
    };
    if spec.id == "claude-code" {
        let reached: Vec<&str> = curated
            .iter()
            .filter_map(|c| listed.iter().find(|l| l.id == c.id))
            .map(|l| l.resolves_to.as_deref().unwrap_or(&l.id))
            .collect();
        let mut models: Vec<Model> = curated
            .iter()
            .map(|c| match listed.iter().find(|l| l.id == c.id) {
                Some(l) => Model {
                    description: c.description.clone(),
                    ..entry(l, c.default)
                },
                None => c.clone(),
            })
            .collect();
        for l in &listed {
            let offered = models
                .iter()
                .any(|m| same_model(&m.id, &l.id) || m.name == l.name)
                || reached.contains(&l.id.as_str());
            if !offered {
                models.push(entry(l, false));
            }
        }
        return models;
    }
    if spec.group == Group::Key {
        // A key's list adds to Desk's table: the models Desk words keep its
        // words and place, and a model released since follows them by the
        // name its provider gives it. Nothing Desk offers is taken away, as
        // an account can use models its listing doesn't show.
        let mut models = curated;
        for l in &listed {
            if !models.iter().any(|m| same_model(&m.id, &l.id)) {
                models.push(entry(l, false));
            }
        }
        return models;
    }
    let default = curated
        .iter()
        .find(|c| c.default && listed.iter().any(|l| l.id == c.id))
        .map(|c| c.id.clone())
        .or_else(|| listed.first().map(|l| l.id.clone()));
    listed
        .iter()
        .map(|l| {
            let model = entry(l, default.as_deref() == Some(l.id.as_str()));
            match curated.iter().find(|c| c.id == l.id) {
                Some(c) => Model {
                    name: c.name.clone(),
                    description: c.description.clone(),
                    ..model
                },
                None => model,
            }
        })
        .collect()
}

/// `id` without a version suffix as Bedrock words one: `-v1:0` or `:0`.
fn unversioned(id: &str) -> &str {
    let id = match id.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    };
    match id.rsplit_once("-v") {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => id,
    }
}

/// Whether two ids name one model, differing at most by a version suffix
/// (`anthropic.claude-opus-5-5` and `anthropic.claude-opus-5-5-v1:0`).
fn same_model(a: &str, b: &str) -> bool {
    a == b || unversioned(a) == unversioned(b)
}

/// The efforts `listed` takes that the hub honours on its provider, when
/// the plan says which it takes; the one it starts at, when it says that
/// too.
fn listed_efforts(listed: &Listed, honoured: &[&'static str]) -> Efforts {
    if listed.efforts.is_empty() {
        // A plan lists no levels for a model that takes none.
        return Efforts::of(Vec::new());
    }
    let efforts: Vec<&'static str> = honoured
        .iter()
        .copied()
        .filter(|e| listed.efforts.iter().any(|l| l == e))
        .collect();
    let mut chosen = Efforts::of(efforts);
    if let Some(own) = listed
        .default_effort
        .as_deref()
        .and_then(|d| chosen.efforts.iter().copied().find(|e| *e == d))
    {
        chosen.default_effort = Some(own);
    }
    chosen
}

/// A local server's models, the first the default.
fn installed(provider: &str, models: Vec<String>) -> Vec<Model> {
    models
        .into_iter()
        .enumerate()
        .map(|(i, id)| Model {
            name: id.clone(),
            description: "Installed on the server".into(),
            default: i == 0,
            provider: provider.into(),
            efforts: None,
            fast: None,
            id,
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub(crate) struct Catalog {
    providers: Vec<Entry>,
}

impl Catalog {
    pub(crate) async fn of(accounts: &dyn Accounts) -> Self {
        let mut providers = Vec::with_capacity(PROVIDERS.len());
        for spec in PROVIDERS {
            providers.push(Entry::of(spec, accounts).await);
        }
        Self { providers }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ConnectBody {
    #[serde(default)]
    pub(crate) fields: Fields,
}

/// `fields` trimmed and held to `spec`'s: every required one given, none it
/// does not take, blanks dropped.
pub fn checked(spec: &Spec, fields: Fields) -> Result<Fields, AccountError> {
    if spec.connect.kind == ConnectKind::Login {
        return Err(AccountError::Invalid(format!(
            "{} connects by signing in from Desk, not with fields",
            spec.name
        )));
    }
    let ways: Vec<&[Field]> = match spec.connect.ways {
        [] => vec![spec.connect.fields],
        ways => ways.iter().map(|w| w.fields).collect(),
    };
    let takes = |way: &[Field], name: &str| way.iter().any(|f| f.name == name);
    if let Some(unknown) = fields
        .keys()
        .find(|name| !ways.iter().any(|way| takes(way, name)))
    {
        return Err(AccountError::Invalid(format!(
            "{} takes no field named {unknown:?}",
            spec.name
        )));
    }
    // The first way that takes every field given.
    let Some(way) = ways
        .iter()
        .copied()
        .find(|way| fields.keys().all(|name| takes(way, name)))
    else {
        let labels: Vec<&str> = spec.connect.ways.iter().map(|w| w.label).collect();
        return Err(AccountError::Invalid(format!(
            "{} connects one way at a time: {}",
            spec.name,
            labels.join(" or ")
        )));
    };
    let given: Fields = fields
        .into_iter()
        .map(|(name, value)| (name, value.trim().to_string()))
        .filter(|(_, value)| !value.is_empty())
        .collect();
    let missing: Vec<&str> = way
        .iter()
        .filter(|f| f.required && !given.contains_key(f.name))
        .map(|f| f.label)
        .collect();
    match missing.is_empty() {
        true => Ok(given),
        false => Err(AccountError::Invalid(format!(
            "{} needs {}",
            spec.name,
            missing.join(" and ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::{to_bytes, Body};
    use axum::http::{Method, Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::api::providers_router;
    use crate::testing::Bearer;

    /// Holds what was connected, in memory.
    #[derive(Default)]
    struct Held(Mutex<BTreeMap<&'static str, Fields>>);

    #[async_trait]
    impl Accounts for Held {
        async fn standing(&self, spec: &Spec) -> Standing {
            Standing {
                connected: self.0.lock().unwrap().contains_key(spec.id),
                installed: match spec.id {
                    "ollama" => vec!["llama4:8b".into(), "qwen3.8:27b".into()],
                    _ => Vec::new(),
                },
                listed: None,
            }
        }

        fn efforts(&self, provider: &str) -> Vec<&'static str> {
            match provider {
                "claude-code" => vec!["low", "medium", "high", "max"],
                "anthropic" | "bedrock" => vec!["low", "medium", "high", "xhigh", "max"],
                _ => Vec::new(),
            }
        }

        fn model_efforts(&self, provider: &str, model: &str) -> Vec<&'static str> {
            match model.starts_with("anthropic.") || provider != "bedrock" {
                true => self.efforts(provider),
                false => Vec::new(),
            }
        }

        async fn connect(&self, spec: &Spec, fields: &Fields) -> Result<(), AccountError> {
            self.0.lock().unwrap().insert(spec.id, fields.clone());
            Ok(())
        }

        async fn disconnect(&self, spec: &Spec) -> Result<(), AccountError> {
            self.0.lock().unwrap().remove(spec.id);
            Ok(())
        }
    }

    async fn call(
        accounts: Arc<Held>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer hub-token")
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = providers_router(Arc::new(Bearer), accounts)
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let body = match bytes.is_empty() {
            true => Value::Null,
            false => serde_json::from_slice(&bytes).unwrap(),
        };
        (status, body)
    }

    #[test]
    fn plans_come_first_then_keys_then_local_servers_and_codex_is_not_offered() {
        let ids: Vec<&str> = PROVIDERS.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            [
                "claude-code",
                "chatgpt",
                "anthropic",
                "bedrock",
                "vertex-anthropic",
                "gemini",
                "openrouter",
                "grok",
                "zai",
                "cerebras",
                "ollama",
                "vllm",
            ]
        );
        let groups: Vec<Group> = PROVIDERS.iter().map(|s| s.group).collect();
        assert!(groups.windows(2).all(|w| w[0] as u8 <= w[1] as u8));
    }

    #[test]
    fn every_curated_list_has_exactly_one_default_and_no_repeats() {
        for spec in PROVIDERS.iter().filter(|s| !s.lists_installed()) {
            let defaults = spec.models.iter().filter(|(_, d)| *d).count();
            assert_eq!(defaults, 1, "{}", spec.id);
            let mut ids: Vec<&str> = spec.model_ids().collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), spec.models.len(), "{}", spec.id);
        }
    }

    #[test]
    fn the_plans_offer_fable_and_astra_with_sonnet_and_sol_the_defaults() {
        let models = |id| spec(id).unwrap().curated();
        let claude = models("claude-code");
        assert_eq!(
            claude
                .iter()
                .map(|m| (m.id.as_str(), m.name.as_str(), m.default))
                .collect::<Vec<_>>(),
            [
                ("fable", "Fable 5.2", false),
                ("opus", "Opus 5.5", false),
                ("sonnet", "Sonnet 5.5", true),
                ("haiku", "Haiku 5.5", false),
            ]
        );
        let chatgpt = models("chatgpt");
        assert_eq!(
            chatgpt
                .iter()
                .map(|m| (m.id.as_str(), m.default))
                .collect::<Vec<_>>(),
            [
                ("gpt-6-astra", false),
                ("gpt-6.1-sol", true),
                ("gpt-6-sol", false),
                ("gpt-6-luna", false),
                ("gpt-5.6-sol", false),
                ("gpt-5.6-terra", false),
                ("gpt-5.6-luna", false),
            ],
            "Terra is offered when the plan can't be asked"
        );
    }

    #[test]
    fn a_model_is_routed_through_its_family_where_the_hub_routes_by_family() {
        assert_eq!(route("claude-code", "fable"), "claude-code");
        assert_eq!(
            route("bedrock", "anthropic.claude-opus-5-5"),
            "bedrock:anthropic"
        );
        assert_eq!(route("bedrock", "deepseek.v3.2"), "bedrock:deepseek");
        assert_eq!(route("openrouter", "moonshotai/kimi-k3"), "openrouter:kimi");
        assert_eq!(route("openrouter", "z-ai/glm-5.3"), "openrouter:glm");
        assert_eq!(route("cerebras", "gpt-oss-120b"), "cerebras:gpt");
        assert_eq!(
            route("vertex-anthropic", "claude-opus-5-5"),
            "vertex:anthropic"
        );
        assert_eq!(route("ollama", "llama4:8b"), "ollama");
    }

    #[test]
    fn bedrock_connects_with_keys_or_an_aws_profile_one_at_a_time() {
        let bedrock = spec("bedrock").unwrap();
        let fields = |pairs: &[(&str, &str)]| -> Fields {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let ways: Vec<&str> = bedrock.connect.ways.iter().map(|w| w.id).collect();
        assert_eq!(ways, ["keys", "profile"]);
        assert_eq!(
            bedrock.connect.fields, bedrock.connect.ways[0].fields,
            "a Desk that knows no ways still shows the keys"
        );
        let profile = &bedrock.connect.ways[1].fields[0];
        assert_eq!((profile.name, profile.initial), ("profile", "default"));
        assert!(!profile.secret, "a profile name is not a secret");
        assert_eq!(
            checked(
                bedrock,
                fields(&[("profile", " work-sso "), ("region", "eu-west-2")])
            )
            .unwrap(),
            fields(&[("profile", "work-sso"), ("region", "eu-west-2")])
        );
        assert_eq!(
            checked(bedrock, fields(&[("profile", " ")])).unwrap_err(),
            AccountError::Invalid("Amazon Bedrock needs Profile".into())
        );
        assert_eq!(
            checked(
                bedrock,
                fields(&[("profile", "work"), ("access_key_id", "AKIA")])
            )
            .unwrap_err(),
            AccountError::Invalid(
                "Amazon Bedrock connects one way at a time: Keys or AWS profile".into()
            )
        );
        let json = serde_json::to_value(bedrock.connect).unwrap();
        assert_eq!(json["ways"][1]["label"], "AWS profile");
        assert_eq!(json["ways"][1]["fields"][0]["initial"], "default");
        assert!(json["fields"][0].get("initial").is_none());
        let anthropic = serde_json::to_value(spec("anthropic").unwrap().connect).unwrap();
        assert!(anthropic.get("ways").is_none(), "one way says none");
    }

    #[test]
    fn fields_are_held_to_the_providers_own() {
        let bedrock = spec("bedrock").unwrap();
        let fields = |pairs: &[(&str, &str)]| -> Fields {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let err = checked(
            bedrock,
            fields(&[("access_key_id", "AKIA"), ("region", " ")]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            AccountError::Invalid("Amazon Bedrock needs Secret access key".into())
        );
        let ok = checked(
            bedrock,
            fields(&[
                ("access_key_id", " AKIA "),
                ("secret_access_key", "s3cr3t"),
                ("region", ""),
            ]),
        )
        .unwrap();
        assert_eq!(
            ok,
            fields(&[("access_key_id", "AKIA"), ("secret_access_key", "s3cr3t")])
        );
        let err = checked(spec("anthropic").unwrap(), fields(&[("token", "x")])).unwrap_err();
        assert!(
            err.to_string().contains("no field named \"token\""),
            "{err}"
        );
        let err = checked(spec("claude-code").unwrap(), Fields::new()).unwrap_err();
        assert!(err.to_string().contains("signing in"), "{err}");
    }

    /// A hub whose plans list their models now.
    struct Listing;

    fn listed(id: &str, name: &str, efforts: &[&str], fast: Option<&str>) -> Listed {
        Listed {
            id: id.into(),
            name: name.into(),
            description: format!("{name} as the plan says"),
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
            fast: fast.map(str::to_string),
            ..Listed::default()
        }
    }

    #[async_trait]
    impl Accounts for Listing {
        async fn standing(&self, spec: &Spec) -> Standing {
            let all = ["low", "medium", "high", "xhigh", "max", "ultra"];
            let listed = match spec.id {
                "chatgpt" => Some(vec![
                    Listed {
                        default_effort: Some("low".into()),
                        ..listed(
                            "gpt-6.1-sol",
                            "GPT-6.1-Sol",
                            &all,
                            Some("2x speed, increased usage"),
                        )
                    },
                    listed(
                        "gpt-6-astra",
                        "GPT-6-Astra",
                        &all,
                        Some("2x speed, increased usage"),
                    ),
                    listed(
                        "gpt-6.2-nova",
                        "GPT-6.2-Nova",
                        &["low", "medium"],
                        Some("1.5x speed"),
                    ),
                    listed("gpt-6-luna", "GPT-6-Luna", &["low", "medium"], None),
                ]),
                "claude-code" => Some(vec![
                    Listed {
                        resolves_to: Some("claude-opus-5-5".into()),
                        ..listed("opus", "Opus 5.5", &all, None)
                    },
                    listed("claude-fable-5-1", "Fable 5.1", &all, None),
                    Listed {
                        resolves_to: Some("claude-sonnet-5-5".into()),
                        ..listed("sonnet", "Sonnet 5.5", &all, None)
                    },
                    listed("claude-sonnet-5-5", "Sonnet 5.5", &all, None),
                    listed("claude-fable-5-2", "Fable 5.2", &all, None),
                    listed("claude-haiku-4-5-20251001", "Haiku 4.5", &[], None),
                ]),
                "anthropic" => Some(vec![
                    listed("claude-fable-5-3", "Claude Fable 5.3", &[], None),
                    listed("claude-opus-5-5", "Opus 5.5 as the account says", &[], None),
                ]),
                "bedrock" => Some(vec![
                    listed("anthropic.claude-fable-5-3", "Claude Fable 5.3", &[], None),
                    listed("anthropic.claude-sonnet-5-5", "Sonnet", &[], None),
                    listed("anthropic.claude-opus-5-5-v1:0", "Opus", &[], None),
                ]),
                _ => None,
            };
            Standing {
                connected: true,
                installed: Vec::new(),
                listed,
            }
        }

        fn efforts(&self, provider: &str) -> Vec<&'static str> {
            match provider {
                "claude-code" => vec!["low", "medium", "high", "max"],
                "chatgpt" => vec!["low", "medium", "high", "xhigh"],
                _ => Vec::new(),
            }
        }

        async fn connect(&self, _: &Spec, _: &Fields) -> Result<(), AccountError> {
            Ok(())
        }

        async fn disconnect(&self, _: &Spec) -> Result<(), AccountError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_chatgpt_plan_offers_what_it_lists_now_worded_as_desk_knows_it() {
        let entry = Entry::of(spec("chatgpt").unwrap(), &Listing).await;
        let shown: Vec<(&str, &str, bool, Option<&str>)> = entry
            .models
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str(), m.default, m.fast.as_deref()))
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "gpt-6.1-sol",
                    "GPT-6.1 Sol",
                    true,
                    Some("2x speed, increased usage")
                ),
                (
                    "gpt-6-astra",
                    "GPT-6 Astra",
                    false,
                    Some("2x speed, increased usage")
                ),
                ("gpt-6.2-nova", "GPT-6.2-Nova", false, Some("1.5x speed")),
                ("gpt-6-luna", "GPT-6 Luna", false, None),
            ],
            "gpt-6-sol, no longer listed, is gone; a new model is offered"
        );
        let nova = &entry.models[2];
        assert_eq!(nova.provider, "chatgpt");
        assert_eq!(nova.description, "GPT-6.2-Nova as the plan says");
        assert_eq!(
            nova.efforts,
            Some(Efforts {
                efforts: vec!["low", "medium"],
                default_effort: Some("medium"),
            })
        );
        // Its own default, and only the levels the hub sends.
        assert_eq!(
            entry.models[0].efforts,
            Some(Efforts {
                efforts: vec!["low", "medium", "high", "xhigh"],
                default_effort: Some("low"),
            })
        );
        assert_eq!(entry.models[1].efforts, None, "the provider's own levels");
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["models"][2]["fast"], "1.5x speed");
        assert!(json["models"][3].get("fast").is_none());
    }

    #[tokio::test]
    async fn a_claude_plan_keeps_its_aliases_and_adds_what_no_alias_offers() {
        let entry = Entry::of(spec("claude-code").unwrap(), &Listing).await;
        let shown: Vec<(&str, &str, bool)> = entry
            .models
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str(), m.default))
            .collect();
        assert_eq!(
            shown,
            [
                ("fable", "Fable 5.2", false),
                ("opus", "Opus 5.5", false),
                ("sonnet", "Sonnet 5.5", true),
                ("haiku", "Haiku 5.5", false),
                ("claude-fable-5-1", "Fable 5.1", false),
                ("claude-haiku-4-5-20251001", "Haiku 4.5", false),
            ],
            "Fable 5.2 is the fable alias's; the previous Fable is offered by its id"
        );
        assert_eq!(entry.models[2].description, SONNET, "Desk's words kept");
        assert_eq!(
            entry.models[5].efforts,
            Some(Efforts {
                efforts: Vec::new(),
                default_effort: None,
            }),
            "a model that takes no effort has none"
        );
        assert_eq!(
            entry.models[4].efforts, None,
            "the plan's levels the hub takes"
        );
    }

    /// [`Listing`] with a ChatGPT plan whose catalogue lists both Terras.
    struct WithTerra;

    #[async_trait]
    impl Accounts for WithTerra {
        async fn standing(&self, spec: &Spec) -> Standing {
            let all = ["low", "medium", "high"];
            Standing {
                connected: true,
                installed: Vec::new(),
                listed: Some(vec![
                    listed("gpt-6.1-sol", "GPT-6.1-Sol", &all, None),
                    listed("gpt-6.1-terra", "GPT-6.1 Terra", &all, None),
                    listed("gpt-5.6-terra", "GPT-5.6-Terra", &all, None),
                ])
                .filter(|_| spec.id == "chatgpt"),
            }
        }

        fn efforts(&self, provider: &str) -> Vec<&'static str> {
            Listing.efforts(provider)
        }

        async fn connect(&self, _: &Spec, _: &Fields) -> Result<(), AccountError> {
            Ok(())
        }

        async fn disconnect(&self, _: &Spec) -> Result<(), AccountError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_chatgpt_plan_that_lists_terra_offers_it_in_every_generation_it_lists() {
        let entry = Entry::of(spec("chatgpt").unwrap(), &WithTerra).await;
        let ids: Vec<&str> = entry.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6.1-sol", "gpt-6.1-terra", "gpt-5.6-terra"]);
        let terra = &entry.models[1];
        assert_eq!(terra.name, "GPT-6.1 Terra", "worded as the plan words it");
        assert_eq!(terra.provider, "chatgpt");
        assert!(entry.models[0].default, "Sol stays the default");
    }

    #[tokio::test]
    async fn a_key_adds_what_its_account_lists_to_desks_table_and_takes_nothing_away() {
        let anthropic = Entry::of(spec("anthropic").unwrap(), &Listing).await;
        let shown: Vec<(&str, &str, bool)> = anthropic
            .models
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str(), m.default))
            .collect();
        assert_eq!(
            shown,
            [
                ("claude-fable-5-2", "Claude Fable 5.2", false),
                ("claude-fable-5-1", "Claude Fable 5.1", false),
                ("claude-opus-5-5", "Claude Opus 5.5", false),
                ("claude-sonnet-5-5", "Claude Sonnet 5.5", true),
                ("claude-haiku-5-5", "Claude Haiku 5.5", false),
                ("claude-fable-5-3", "Claude Fable 5.3", false),
            ],
            "Desk's names and order kept; the model released since follows"
        );
        assert_eq!(anthropic.models[2].description, OPUS, "Desk's words kept");
        assert_eq!(
            anthropic.models[5].description, "Claude Fable 5.3 as the plan says",
            "a new model has its provider's words"
        );

        let bedrock = Entry::of(spec("bedrock").unwrap(), &Listing).await;
        let new = bedrock
            .models
            .iter()
            .find(|m| m.id == "anthropic.claude-fable-5-3")
            .expect("offered");
        assert_eq!(
            new.provider, "bedrock:anthropic",
            "routed by its family, never a region"
        );
        assert!(
            bedrock.models.iter().any(|m| m.id == "zai.glm-5"),
            "a curated model the listing lacks stays"
        );
        assert_eq!(
            bedrock
                .models
                .iter()
                .filter(|m| m.id == "anthropic.claude-sonnet-5-5")
                .count(),
            1
        );
        assert!(
            bedrock
                .models
                .iter()
                .all(|m| m.id != "anthropic.claude-opus-5-5-v1:0"),
            "a model listed with a version suffix is the one Desk already offers"
        );
    }

    #[test]
    fn ids_that_differ_only_by_a_version_suffix_are_one_model() {
        assert!(same_model(
            "anthropic.claude-opus-5-5",
            "anthropic.claude-opus-5-5-v1:0"
        ));
        assert!(same_model(
            "qwen.qwen3-coder-480b-a35b-v1:0",
            "qwen.qwen3-coder-480b-a35b"
        ));
        assert!(!same_model(
            "anthropic.claude-opus-5-5",
            "anthropic.claude-opus-5-6"
        ));
        assert!(!same_model("deepseek.v3.2", "deepseek.v3"));
    }

    #[tokio::test]
    async fn a_plan_that_couldnt_be_asked_offers_desks_own_table() {
        let entry = Entry::of(spec("chatgpt").unwrap(), &Held::default()).await;
        let ids: Vec<&str> = entry.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gpt-6-astra",
                "gpt-6.1-sol",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna"
            ]
        );
        assert!(entry.models.iter().all(|m| m.fast.is_none()));
    }

    #[tokio::test]
    async fn lists_every_provider_with_its_standing_models_and_efforts() {
        let held = Arc::new(Held::default());
        let (status, body) = call(held, Method::GET, "/api/desk/providers", None).await;
        assert_eq!(status, StatusCode::OK);
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), PROVIDERS.len());
        let claude = &providers[0];
        assert_eq!(claude["id"], "claude-code");
        assert_eq!(claude["name"], "Claude plan");
        assert_eq!(claude["group"], "plan");
        assert_eq!(claude["connected"], false);
        assert_eq!(claude["connect"]["kind"], "login");
        assert_eq!(claude["efforts"], json!(["low", "medium", "high", "max"]));
        assert_eq!(claude["defaultEffort"], "medium");
        assert_eq!(
            claude["models"][2],
            json!({
                "id": "sonnet",
                "name": "Sonnet 5.5",
                "description": "Fast and strong — the everyday choice",
                "default": true,
                "provider": "claude-code",
            })
        );
        let grok = providers.iter().find(|p| p["id"] == "grok").unwrap();
        assert_eq!(grok["efforts"], json!([]));
        assert_eq!(grok["defaultEffort"], Value::Null);
        assert_eq!(
            grok["connect"]["fields"],
            json!([{
                "name": "api_key", "label": "API key", "secret": true,
                "required": true, "multiline": false, "placeholder": "",
            }])
        );
        let bedrock = providers.iter().find(|p| p["id"] == "bedrock").unwrap();
        assert_eq!(bedrock["defaultEffort"], "medium");
        let opus = &bedrock["models"][2];
        assert_eq!(opus["id"], "anthropic.claude-opus-5-5");
        assert!(
            opus.get("efforts").is_none() && opus.get("defaultEffort").is_none(),
            "a model that takes the provider's efforts carries none of its own: {opus}"
        );
        let deepseek = bedrock["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "deepseek.v3.2")
            .unwrap();
        assert_eq!(deepseek["efforts"], json!([]));
        assert_eq!(
            deepseek["defaultEffort"],
            Value::Null,
            "said, so the provider's is not read in its place"
        );
        assert!(deepseek.as_object().unwrap().contains_key("defaultEffort"));
        let ollama = providers.iter().find(|p| p["id"] == "ollama").unwrap();
        assert_eq!(ollama["group"], "local");
        assert_eq!(ollama["connect"]["kind"], "probe");
        assert_eq!(ollama["models"][0]["id"], "llama4:8b");
        assert_eq!(ollama["models"][0]["default"], true);
        assert_eq!(ollama["models"][1]["default"], false);
    }

    #[tokio::test]
    async fn connecting_answers_the_refreshed_entry_and_disconnecting_forgets_it() {
        let held = Arc::new(Held::default());
        let (status, body) = call(
            Arc::clone(&held),
            Method::POST,
            "/api/desk/providers/anthropic/connect",
            Some(json!({ "fields": { "api_key": "sk-ant-api03-secret" } })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "anthropic");
        assert_eq!(body["connected"], true);
        assert!(!body.to_string().contains("sk-ant-api03"), "{body}");
        assert_eq!(
            held.0.lock().unwrap()["anthropic"]["api_key"],
            "sk-ant-api03-secret"
        );

        let (status, body) = call(
            Arc::clone(&held),
            Method::DELETE,
            "/api/desk/providers/anthropic",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["connected"], false);
        assert!(held.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_blank_field_an_unknown_provider_or_a_plan_is_refused() {
        let held = Arc::new(Held::default());
        let (status, body) = call(
            Arc::clone(&held),
            Method::POST,
            "/api/desk/providers/gemini/connect",
            Some(json!({ "fields": { "api_key": "   " } })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Google Gemini needs API key");
        let (status, _) = call(
            Arc::clone(&held),
            Method::POST,
            "/api/desk/providers/codex/connect",
            Some(json!({ "fields": {} })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            Arc::clone(&held),
            Method::POST,
            "/api/desk/providers/chatgpt/connect",
            Some(json!({ "fields": {} })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            Arc::clone(&held),
            Method::POST,
            "/api/desk/providers/gemini/connect",
            Some(json!({ "nope": 1 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(held.0.lock().unwrap().is_empty());
    }

    #[test]
    fn vertex_takes_a_key_or_the_persons_google_login() {
        let vertex = spec("vertex-anthropic").unwrap();
        let key = vertex.connect.fields[0];
        assert_eq!(key.name, "service_account_json");
        assert!(!key.required && key.secret && key.multiline);
        assert!(vertex.connect.fields.iter().all(|f| !f.required));
        assert!(vertex
            .connect
            .hint
            .contains("empty to use your Google login"));
        assert!(vertex
            .connect
            .hint
            .contains("gcloud auth application-default login"));
        let given = checked(
            vertex,
            [("project".to_string(), "acme-ai".to_string())].into(),
        )
        .unwrap();
        assert_eq!(given["project"], "acme-ai");
    }

    #[tokio::test]
    async fn needs_the_hub_token() {
        let request = Request::get("/api/desk/providers")
            .header("authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let response = providers_router(Arc::new(Bearer), Arc::new(Held::default()))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
