//! Fixtures shared by the subscription tests and the service tests that run
//! agents on a mock ChatGPT backend.

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use base64::Engine;
use chrono::{TimeDelta, Utc};
use wiremock::ResponseTemplate;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
};
use workforce_common::subscription::{AgentOwner, CredentialTier, LoginOwner, SubscriptionKind};

use super::resolve::{Credential, Login, LoginKey, VaultSlot};
use crate::vault::Vault;

pub const TEXT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../agent-runtime/src/llm/testdata/chatgpt/text_reasoning.sse"
));
pub const SKY: &str = "The sky is blue because air molecules scatter \
    shorter-wavelength blue sunlight more than other colors, making blue light come from \
    all directions.";

/// A recorded exchange (`HTTP <status>`, headers, blank line, body) as the
/// backend's response.
pub fn recorded(raw: &str) -> ResponseTemplate {
    let (head, body) = raw.split_once("\n\n").expect("head and body");
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.strip_prefix("HTTP "))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    lines
        .filter_map(|l| l.split_once(':'))
        .filter(|(_, v)| !v.trim().is_empty())
        .fold(ResponseTemplate::new(status), |t, (k, v)| {
            t.insert_header(k.trim(), v.trim())
        })
        .set_body_string(body)
}

/// An unsigned JWT carrying `claims`.
pub fn jwt(claims: serde_json::Value) -> String {
    let encode = |v: &serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
    };
    format!(
        "{}.{}.c2lnbmF0dXJlLWJ5dGVz",
        encode(&serde_json::json!({"alg": "RS256", "typ": "JWT"})),
        encode(&claims)
    )
}

/// An access token that stays valid for the test, tagged so requests show
/// which login they carried.
pub fn access(tag: &str) -> String {
    jwt(serde_json::json!({"exp": (Utc::now() + TimeDelta::hours(1)).timestamp(), "tag": tag}))
}

/// `member`'s ChatGPT login on `account`, as `codex login` writes it.
pub fn member_login(account: &str, member: &str, access_tag: &str, refresh_token: &str) -> String {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": jwt(serde_json::json!({"sub": member})),
            "access_token": access(access_tag),
            "refresh_token": refresh_token,
            "account_id": account
        }
    })
    .to_string()
}

/// A ChatGPT login on `account`, as `codex login` writes it.
pub fn login(account: &str, access_tag: &str, refresh_token: &str) -> String {
    member_login(account, "user-1", access_tag, refresh_token)
}

/// A Claude login holding `token`: an agent's shared login, in a vault of its
/// own that outlives the test.
pub fn claude_login(token: &str) -> Login {
    static VAULT: LazyLock<(tempfile::TempDir, Arc<Vault>)> = LazyLock::new(|| {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(Vault::open(&dir.path().join("logins.enc"), "test-key").unwrap());
        (dir, vault)
    });
    let credential = Credential::Token(token.to_string());
    Login {
        kind: SubscriptionKind::Claude,
        tier: CredentialTier::AgentShared,
        owner: LoginOwner::Agent(AgentOwner::Unowned),
        key: LoginKey::of(&credential).expect("a Claude token has a key"),
        credential,
        slot: VaultSlot {
            vault: Arc::clone(&VAULT.1),
            key: SubscriptionKind::Claude.vault_key().to_string(),
        },
    }
}

/// A metered provider that answers any model, recording each one asked for.
#[derive(Default)]
pub struct Metered(pub parking_lot::Mutex<Vec<String>>);

#[async_trait]
impl LlmProvider for Metered {
    async fn create_message(
        &self,
        _system_prompt: Option<&str>,
        _messages: &[LlmMessage],
        _tools: &[LlmToolDef],
        model: Option<&str>,
        _max_tokens: Option<u32>,
        _enable_cache: bool,
    ) -> eyre::Result<LlmResponse> {
        self.0.lock().push(model.unwrap_or_default().to_string());
        Ok(LlmResponse {
            content: vec![ContentBlock::Text {
                text: "metered".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            model: model.unwrap_or_default().to_string(),
        })
    }
}

/// Answers a refresh with `rt-1`, as the real endpoint would, and meanwhile
/// puts a directory where the vault file was, so the vault cannot store it.
pub struct BlocksTheVault {
    pub vault_file: std::path::PathBuf,
    pub aside: std::path::PathBuf,
}

impl wiremock::Respond for BlocksTheVault {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        std::fs::rename(&self.vault_file, &self.aside).unwrap();
        std::fs::create_dir(&self.vault_file).unwrap();
        std::fs::write(self.vault_file.join("occupied"), "").unwrap();
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": jwt(serde_json::json!({
                "exp": (Utc::now() + TimeDelta::hours(1)).timestamp()
            })),
            "refresh_token": "rt-1",
        }))
    }
}
