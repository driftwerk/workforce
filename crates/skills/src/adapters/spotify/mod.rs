//! Spotify skill — agent-callable playback control.
//!
//! Auth is 3-legged OAuth 2.0: an admin configures one OAuth app
//! (`skills/spotify/oauth_client/*`), each user connects their account through
//! the hub-hosted web-redirect flow (`spotify.setup`), and tokens are minted
//! and refreshed by the shared `workforce-oauth-client` `TokenStore`.
//!
//! This exists so the assistant can be asked for music out loud. That shapes
//! two decisions that look like deviations from the other adapters:
//!
//! * Every tool is `allow`, not `require_approval`. See [`tools`].
//! * Commands prefer a named speaker over whatever device Spotify last used,
//!   because "play something" said to a speaker means *that* speaker. See
//!   [`client::choose_device`].

pub mod client;
pub mod provider;
pub mod setup;
pub mod token_store;
pub mod tools;

use std::sync::Arc;

use eyre::{eyre, Result};
use workforce_agent::tools::Tool;
use workforce_common::settings::SecretStore;
use workforce_oauth_client::OAuthClientCreds;

use crate::definition::{CredentialRequirement, SkillDefinition, SkillRequirements};

use client::SpotifyContext;

/// Vault prefix for the admin-configured OAuth app credentials.
pub const OAUTH_CLIENT_PREFIX: &str = "skills/spotify/oauth_client/";

/// Device commands target when it is available.
///
/// Defaults to the satellite's librespot name, because the speaker in the room
/// is what someone talking to the room means. An operator running a differently
/// named receiver sets `skills/spotify/oauth_client/device` to match.
pub const DEFAULT_DEVICE: &str = "Allan";

fn oauth_field(store: &dyn SecretStore, field: &str) -> Option<String> {
    store.get(&format!("{OAUTH_CLIENT_PREFIX}{field}"))
}

/// The speaker playback commands prefer.
pub fn read_preferred_device(store: &dyn SecretStore) -> String {
    oauth_field(store, "device")
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| DEFAULT_DEVICE.to_string())
}

/// Read the OAuth app credentials. `redirect_uri` is included when present —
/// the web flow needs it; refresh and data calls do not.
pub fn read_client_creds(store: &dyn SecretStore) -> Result<OAuthClientCreds> {
    let client_id = oauth_field(store, "client_id")
        .ok_or_else(|| eyre!("Spotify is not configured: set {OAUTH_CLIENT_PREFIX}client_id"))?;
    let client_secret = oauth_field(store, "client_secret").ok_or_else(|| {
        eyre!("Spotify is not configured: set {OAUTH_CLIENT_PREFIX}client_secret")
    })?;
    Ok(OAuthClientCreds {
        client_id,
        client_secret: Some(client_secret),
        redirect_uri: oauth_field(store, "redirect_uri"),
    })
}

/// The Spotify skill — playback tools resolving the caller's tokens per call.
pub struct SpotifySkill {
    context: Arc<SpotifyContext>,
}

impl SpotifySkill {
    /// Build from a vault that resolves credentials per request. Errors when
    /// the admin OAuth app is unconfigured.
    pub fn with_store(store: Arc<dyn SecretStore>) -> Result<Self> {
        let creds = read_client_creds(store.as_ref())?;
        let device = read_preferred_device(store.as_ref());
        let http = client::build_http_client()?;
        let context = Arc::new(SpotifyContext::new(store, http, creds, device));
        Ok(Self { context })
    }

    /// All agent-callable playback tools.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        tools::build_tools(Arc::clone(&self.context))
    }

    /// Tool names this skill provides, in registration order — the playback
    /// tools plus the separately-built `spotify.setup` onboarding tool.
    pub fn tool_names() -> Vec<&'static str> {
        let mut names = tools::tool_names();
        names.push(setup::SETUP_TOOL_NAME);
        names
    }

    /// Static skill definition (metadata + policies).
    pub fn definition() -> SkillDefinition {
        use crate::definition::{SkillPolicy, SkillTool};
        let (mut tools, mut default_policies) = tools::skill_tools_and_policies();
        // The setup tool manages only the caller's own connection, so `allow`.
        tools.push(SkillTool {
            name: setup::SETUP_TOOL_NAME.to_string(),
            description: setup::description().to_string(),
            input_schema: setup::input_schema(),
        });
        default_policies.push(SkillPolicy {
            tool: setup::SETUP_TOOL_NAME.to_string(),
            action: "allow".to_string(),
        });
        SkillDefinition {
            name: "spotify-integration".to_string(),
            version: "1.0.0".to_string(),
            description: "Control Spotify playback — play, pause, skip, volume, and what is \
                          playing — on a connected account, preferring a named speaker."
                .to_string(),
            author: "workforce".to_string(),
            license: "MIT".to_string(),
            tools,
            event_sources: Vec::new(),
            default_policies,
            memory_schemas: Vec::new(),
            requirements: SkillRequirements {
                credentials: vec![
                    CredentialRequirement {
                        name: "spotify_client_id".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Spotify OAuth app client id".to_string(),
                    },
                    CredentialRequirement {
                        name: "spotify_client_secret".to_string(),
                        r#type: "oauth2".to_string(),
                        description: "Spotify OAuth app client secret".to_string(),
                    },
                ],
                config: Vec::new(),
            },
            dependencies: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::InMem;

    fn configured_store() -> Arc<InMem> {
        InMem::from(&[
            ("skills/spotify/oauth_client/client_id", "cid"),
            ("skills/spotify/oauth_client/client_secret", "secret"),
        ])
    }

    #[test]
    fn with_store_errors_without_credentials() {
        let err = SpotifySkill::with_store(InMem::from(&[]) as Arc<dyn SecretStore>)
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("not configured"), "{err}");
    }

    #[test]
    fn with_store_builds_and_exposes_tools() {
        let skill = SpotifySkill::with_store(configured_store() as Arc<dyn SecretStore>).unwrap();
        assert!(!skill.tools().is_empty());
    }

    /// The satellite registers with Spotify under this name, so it is what a
    /// spoken "play something" should reach without any configuration at all.
    #[test]
    fn the_speaker_defaults_to_the_satellite() {
        assert_eq!(read_preferred_device(InMem::from(&[]).as_ref()), "Allan");
    }

    #[test]
    fn an_operator_can_name_a_different_speaker() {
        let store = InMem::from(&[("skills/spotify/oauth_client/device", " Kitchen ")]);
        assert_eq!(read_preferred_device(store.as_ref()), "Kitchen");
        // Blank is not a name, and would match no device at all.
        let blank = InMem::from(&[("skills/spotify/oauth_client/device", "   ")]);
        assert_eq!(read_preferred_device(blank.as_ref()), "Allan");
    }

    #[test]
    fn definition_lists_all_tools_with_policies() {
        let def = SpotifySkill::definition();
        assert_eq!(def.name, "spotify-integration");
        assert_eq!(def.default_policies.len(), def.tools.len());
        let names: Vec<&str> = def.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"spotify.play"));
        assert!(names.contains(&"spotify.setup"));
    }

    #[test]
    fn tool_names_matches_registered() {
        let skill = SpotifySkill::with_store(configured_store() as Arc<dyn SecretStore>).unwrap();
        let data: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(data, tools::tool_names());
        let all = SpotifySkill::tool_names();
        assert_eq!(all.len(), data.len() + 1);
        assert_eq!(*all.last().unwrap(), setup::SETUP_TOOL_NAME);
    }
}
