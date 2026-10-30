//! How much of this Mac's plans its own Claude Code and Codex report used:
//! the readings the desk passes to the hub, and who the Mac is signed in as.

use serde::Deserialize;
use serde_json::Value;

/// Which of this Mac's CLIs a reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacPlan {
    Claude,
    Codex,
}

/// Who a CLI on this Mac is signed in as: the ids its account is known by,
/// raw (the hub keys them), and the plan's name when it says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacAccount {
    pub ids: Vec<String>,
    pub plan: Option<String>,
}

/// Where the desk reports this Mac's readings: the hub normalises and keeps
/// them.
pub trait PlanSink: Send + Sync {
    /// `payload` as `plan`'s CLI reported it: Claude Code's `rateLimits`,
    /// Codex's rate-limit snapshot.
    fn mac(&self, plan: MacPlan, payload: Value, account: MacAccount);
}

/// What this Mac's CLIs say of their sign-ins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacSignIns {
    /// Claude Code, signed in to a plan.
    pub claude: Option<MacAccount>,
    /// Codex, signed in with ChatGPT.
    pub codex: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStatus {
    #[serde(default)]
    logged_in: bool,
    auth_method: Option<String>,
    org_id: Option<String>,
    email: Option<String>,
    subscription_type: Option<String>,
}

/// The sign-ins that are a plan's: the claude.ai login and a plan's token.
const PLAN_AUTH: [&str; 2] = ["claude.ai", "oauth_token"];

/// Claude Code's `auth status --json`, as the plan it is signed in to.
pub fn claude_sign_in(status: &str) -> Option<MacAccount> {
    let s: ClaudeStatus = serde_json::from_str(status).ok()?;
    let plan_login = s
        .auth_method
        .as_deref()
        .is_some_and(|m| PLAN_AUTH.contains(&m));
    (s.logged_in && plan_login).then(|| MacAccount {
        ids: [s.org_id, s.email].into_iter().flatten().collect(),
        plan: s.subscription_type.filter(|p| !p.trim().is_empty()),
    })
}

/// Whether `codex login status` says it is signed in with ChatGPT.
pub fn codex_signed_in(status: &str) -> bool {
    status
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("logged in using chatgpt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plans_sign_in_is_read_from_auth_status() {
        let status = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
            "email":"person@example.com","orgId":"org-1","orgName":"Example","subscriptionType":"max"}"#;
        assert_eq!(
            claude_sign_in(status),
            Some(MacAccount {
                ids: vec!["org-1".into(), "person@example.com".into()],
                plan: Some("max".into())
            })
        );
        let token = r#"{"loggedIn":true,"authMethod":"oauth_token","apiProvider":"firstParty"}"#;
        assert_eq!(claude_sign_in(token), Some(MacAccount::default()));
        let key = r#"{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}"#;
        assert_eq!(claude_sign_in(key), None, "an API key is no plan");
        assert_eq!(claude_sign_in(r#"{"loggedIn":false}"#), None);
        assert_eq!(claude_sign_in("not json"), None);
    }

    #[test]
    fn codex_is_signed_in_with_chatgpt_or_not() {
        assert!(codex_signed_in("Logged in using ChatGPT\n"));
        assert!(!codex_signed_in("Logged in using an API key - sk-…\n"));
        assert!(!codex_signed_in("Not logged in\n"));
    }
}
