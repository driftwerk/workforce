//! Auto-discovery and skeleton profile creation for new users (DW-146).
//!
//! On first interaction, fetches basic info from Slack and creates a
//! minimal `users/{identity_uuid}.md` profile so the agent has per-user
//! context. The filename is always the canonical identity UUID — friendly
//! names go inside the file, not in the path — so the static lookup in
//! [`workforce_common::identity::AgentIdentity::resolve_user_context`]
//! finds the file using the same key the rest of the per-user system uses.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use eyre::Result;
use tracing::{info, warn};

/// Minimum interval between profile creation attempts for the same identity.
const RATE_LIMIT_SECS: u64 = 60;

/// Basic user info fetched from Slack.
#[derive(Debug, Clone)]
pub struct SlackUserInfo {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub timezone: Option<String>,
    pub title: Option<String>,
}

/// External-platform facts associated with an identity at discovery time.
/// Currently only Slack carries an external ID; the struct generalises so
/// future platforms can populate it without reshaping `ensure_profile`.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryContext {
    pub slack_user_id: Option<String>,
    pub slack_info: Option<SlackUserInfo>,
}

/// Manages automatic creation of per-user profile files.
pub struct UserDiscovery {
    last_attempt: Mutex<HashMap<String, Instant>>,
}

impl UserDiscovery {
    pub fn new() -> Self {
        Self {
            last_attempt: Mutex::new(HashMap::new()),
        }
    }

    /// Ensure a `users/{identity_id}.md` profile exists.
    ///
    /// Returns `Some(path)` if a new profile was created, `None` if one
    /// already exists or the per-identity rate limit blocked the attempt.
    pub fn ensure_profile(
        &self,
        workspace: &Path,
        identity_id: &str,
        context: &DiscoveryContext,
    ) -> Result<Option<PathBuf>> {
        let users_dir = workspace.join("users");
        let path = users_dir.join(format!("{}.md", identity_id));
        if path.exists() {
            return Ok(None);
        }

        {
            let mut limiter = self.last_attempt.lock();
            if let Some(last) = limiter.get(identity_id) {
                if last.elapsed().as_secs() < RATE_LIMIT_SECS {
                    return Ok(None);
                }
            }
            limiter.insert(identity_id.to_string(), Instant::now());
            // Keep the map bounded: an entry older than the rate-limit window
            // no longer gates anything, so drop it. In practice this map only
            // holds identities mid-onboarding (once the profile file exists the
            // `path.exists()` short-circuit above runs first and never touches
            // it), but the sweep makes the bound explicit and is cheap — `n` is
            // the number of users seen in the last RATE_LIMIT_SECS.
            limiter.retain(|_, last| last.elapsed().as_secs() < RATE_LIMIT_SECS);
        }

        std::fs::create_dir_all(&users_dir)?;
        let skeleton = build_skeleton(identity_id, context);
        std::fs::write(&path, &skeleton)?;

        info!(
            identity_id = identity_id,
            path = %path.display(),
            "Created skeleton user profile"
        );

        Ok(Some(path))
    }
}

impl Default for UserDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

fn build_skeleton(identity_id: &str, ctx: &DiscoveryContext) -> String {
    let display_name = ctx
        .slack_info
        .as_ref()
        .and_then(|i| i.display_name.as_deref())
        .unwrap_or(identity_id);

    let opt_line = |label: &str, value: Option<&str>| {
        value
            .filter(|v| !v.trim().is_empty())
            .map(|v| format!("- **{}:** {}", label, v))
    };

    let header_lines = [
        Some(format!("- **Identity ID:** {}", identity_id)),
        opt_line("Slack ID", ctx.slack_user_id.as_deref()),
        opt_line(
            "Email",
            ctx.slack_info.as_ref().and_then(|i| i.email.as_deref()),
        ),
        opt_line(
            "Timezone",
            ctx.slack_info.as_ref().and_then(|i| i.timezone.as_deref()),
        ),
        opt_line(
            "Title",
            ctx.slack_info.as_ref().and_then(|i| i.title.as_deref()),
        ),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n");

    format!(
        "# About {display_name}\n\n\
         {header_lines}\n\n\
         ## Notes about this user\n\n\
         <!-- Edit freely — appears in every prompt as per-user context. -->\n"
    )
}

/// Fetch basic user info from the Slack `users.info` API.
///
/// Returns `None` on any error (network, auth, missing fields) -- profile
/// creation can proceed without Slack info.
pub async fn fetch_slack_user_info(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
) -> Option<SlackUserInfo> {
    let url = format!("https://slack.com/api/users.info?user={}", user_id);
    let resp = client.get(&url).bearer_auth(token).send().await.ok()?;

    let json: serde_json::Value = resp.json().await.ok()?;

    if !json["ok"].as_bool().unwrap_or(false) {
        warn!(
            user_id = user_id,
            error = json["error"].as_str().unwrap_or("unknown"),
            "Slack users.info failed"
        );
        return None;
    }

    let user = &json["user"];
    let profile = &user["profile"];

    Some(SlackUserInfo {
        display_name: profile["display_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| profile["real_name"].as_str().filter(|s| !s.is_empty()))
            .map(String::from),
        email: profile["email"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        timezone: user["tz"].as_str().map(String::from),
        title: profile["title"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
    })
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn slack_ctx() -> DiscoveryContext {
        DiscoveryContext {
            slack_user_id: Some("U03ABCXYZ".into()),
            slack_info: Some(SlackUserInfo {
                display_name: Some("Alice".into()),
                email: Some("alice@example.com".into()),
                timezone: Some("America/New_York".into()),
                title: Some("Engineer".into()),
            }),
        }
    }

    #[test]
    fn skeleton_with_full_slack_info() {
        let md = build_skeleton("uuid-1", &slack_ctx());
        assert!(md.contains("# About Alice"));
        assert!(md.contains("- **Identity ID:** uuid-1"));
        assert!(md.contains("- **Slack ID:** U03ABCXYZ"));
        assert!(md.contains("- **Email:** alice@example.com"));
        assert!(md.contains("- **Timezone:** America/New_York"));
        assert!(md.contains("- **Title:** Engineer"));
    }

    #[test]
    fn skeleton_without_context_uses_identity_as_display_name() {
        let md = build_skeleton("uuid-2", &DiscoveryContext::default());
        assert!(md.contains("# About uuid-2"));
        assert!(md.contains("- **Identity ID:** uuid-2"));
        assert!(!md.contains("Slack ID"));
        assert!(!md.contains("Email"));
        assert!(!md.contains("Timezone"));
        assert!(!md.contains("Title"));
    }

    #[test]
    fn skeleton_omits_blank_slack_fields() {
        let ctx = DiscoveryContext {
            slack_user_id: Some(String::new()),
            slack_info: Some(SlackUserInfo {
                display_name: None,
                email: Some("   ".into()),
                timezone: None,
                title: Some(String::new()),
            }),
        };
        let md = build_skeleton("uuid-3", &ctx);
        assert!(!md.contains("Slack ID"));
        assert!(!md.contains("Email"));
        assert!(!md.contains("Timezone"));
        assert!(!md.contains("Title"));
    }

    #[test]
    fn ensure_profile_writes_uuid_filename() {
        let dir = tempfile::tempdir().unwrap();
        let discovery = UserDiscovery::new();
        let result = discovery
            .ensure_profile(dir.path(), "uuid-789", &slack_ctx())
            .unwrap();
        assert_eq!(result.unwrap(), dir.path().join("users/uuid-789.md"));
        let content = std::fs::read_to_string(dir.path().join("users/uuid-789.md")).unwrap();
        assert!(content.contains("Identity ID:** uuid-789"));
        assert!(content.contains("Slack ID:** U03ABCXYZ"));
    }

    #[test]
    fn ensure_profile_skips_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("users")).unwrap();
        std::fs::write(dir.path().join("users/uuid-789.md"), "existing").unwrap();

        let discovery = UserDiscovery::new();
        let result = discovery
            .ensure_profile(dir.path(), "uuid-789", &DiscoveryContext::default())
            .unwrap();
        assert!(result.is_none());
        let content = std::fs::read_to_string(dir.path().join("users/uuid-789.md")).unwrap();
        assert_eq!(content, "existing");
    }

    #[test]
    fn rate_limiting() {
        let dir = tempfile::tempdir().unwrap();
        let discovery = UserDiscovery::new();

        let first = discovery
            .ensure_profile(dir.path(), "uuid-rate", &DiscoveryContext::default())
            .unwrap();
        assert!(first.is_some());

        std::fs::remove_file(dir.path().join("users/uuid-rate.md")).unwrap();

        let second = discovery
            .ensure_profile(dir.path(), "uuid-rate", &DiscoveryContext::default())
            .unwrap();
        assert!(second.is_none());
    }
}
