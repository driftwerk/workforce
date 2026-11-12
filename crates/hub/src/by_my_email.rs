//! `identity.linear_member_by_my_email` and `identity.jira_user_by_my_email`:
//! the one user on the hub's (shared) Linear or Jira key whose email is the
//! one the caller's sign-in provider, or else Slack, verified ([`crate::verified_email`]). What a Desk board's sync names
//! the person by when the key isn't theirs. They're the identity's, not the
//! adapter's: every agent has them, and one without Linear or Jira doesn't
//! look as though it had.
//!
//! They take no arguments: the email is the caller's own, read from their
//! identity here ([`crate::verified_email::verified_email_state`]), so a caller can
//! never ask about anyone else, and the directory itself never leaves the
//! hub. Each asks the agent's own adapter tool (`linear.members_list` by
//! email, `jira.search_users`) and keeps, here, only exact matches of an
//! active user (on Jira, an `atlassian` account): zero or one `{id, name}`,
//! and why not otherwise.

use std::sync::{Arc, Weak};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use parking_lot::RwLock;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::store::IdentityStore;

use crate::oidc::VerifiedEmail;

/// Which system a tool finds the caller on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum On {
    Linear,
    Jira,
}

impl On {
    fn name(self) -> &'static str {
        match self {
            Self::Linear => "identity.linear_member_by_my_email",
            Self::Jira => "identity.jira_user_by_my_email",
        }
    }

    /// The adapter tool asked, and its arguments for `email`.
    fn lookup(self, email: &str) -> (&'static str, Value) {
        match self {
            Self::Linear => (
                "linear.members_list",
                json!({ "email": email, "limit": 10 }),
            ),
            Self::Jira => (
                "jira.search_users",
                json!({ "query": email, "max_results": 20 }),
            ),
        }
    }
}

pub struct ByMyEmail {
    on: On,
    /// The agent's own tools, where its adapter's lookup is.
    tools: Weak<ToolRegistry>,
    identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

impl ByMyEmail {
    pub fn new(
        on: On,
        tools: &Arc<ToolRegistry>,
        identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
    ) -> Self {
        Self {
            on,
            tools: Arc::downgrade(tools),
            identity_store,
        }
    }
}

/// Why no one was found, as the board's sync reads it.
fn none(why: &str) -> Value {
    json!({ "user": null, "why": why })
}

/// The one active user in `answer` (the adapter's) whose email is exactly
/// `email`; or why not: `none`, `several`, or, when the answer named users
/// whose emails it hid, `email_hidden`.
pub fn pick(on: On, answer: &Value, email: &str) -> Value {
    let (list, id, name) = match on {
        On::Linear => ("members", "id", "name"),
        On::Jira => ("users", "account_id", "display_name"),
    };
    let users: Vec<&Value> = answer[list].as_array().into_iter().flatten().collect();
    let eligible = |u: &&Value| {
        // Only someone who is there now: a missing `active` doesn't count.
        u["active"].as_bool() == Some(true)
            && match on {
                On::Linear => true,
                On::Jira => u["account_type"].as_str() == Some("atlassian"),
            }
    };
    let matches: Vec<&Value> = users
        .iter()
        .copied()
        .filter(eligible)
        .filter(|u| {
            u["email"]
                .as_str()
                .is_some_and(|e| e.trim().eq_ignore_ascii_case(email))
        })
        .collect();
    match matches.as_slice() {
        [one] => {
            let Some(id) = one[id].as_str().filter(|i| !i.trim().is_empty()) else {
                return none("none");
            };
            let shown = match on {
                On::Linear => one["displayName"]
                    .as_str()
                    .filter(|n| !n.trim().is_empty())
                    .or(one[name].as_str()),
                On::Jira => one[name].as_str(),
            };
            json!({ "user": { "id": id, "name": shown.unwrap_or("You") }, "why": null })
        }
        [] if on == On::Jira
            && users
                .iter()
                .copied()
                .filter(eligible)
                .any(|u| u["email"].as_str().is_none_or(|e| e.trim().is_empty())) =>
        {
            none("email_hidden")
        }
        [] => none("none"),
        _ => none("several"),
    }
}

#[async_trait]
impl Tool for ByMyEmail {
    fn name(&self) -> &str {
        self.on.name()
    }

    fn description(&self) -> &str {
        match self.on {
            On::Linear => "The one Linear member with the email your sign-in or Slack verified, as {user: {id, name}, source} (null when there's none, several, or no verified email, with why). Takes no arguments.",
            On::Jira => "The one Jira user with the email your sign-in or Slack verified, as {user: {id, name}, source} (null when there's none, several, Jira hides emails, or no verified email, with why). Takes no arguments.",
        }
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let Some(user) = crate::user_config::current_identity_id() else {
            return Ok(ToolResult::failure(
                "No one is asking: there's no user in this call.",
                ms(),
            ));
        };
        let Some(store) = self.identity_store.read().clone() else {
            return Ok(ToolResult::success(none("no_verified_email"), ms()));
        };
        let identity = match Uuid::parse_str(&user).ok().map(|id| store.get_by_id(id)) {
            Some(Ok(Some(identity))) => identity,
            _ => return Ok(ToolResult::success(none("no_verified_email"), ms())),
        };
        // Whom the person chose to be there comes first.
        if matches!(
            identity.status,
            workforce_identity::types::IdentityStatus::Active
                | workforce_identity::types::IdentityStatus::AutoProvisioned
        ) && identity
            .linked_ids
            .keys()
            .any(|k| crate::chosen_me::is_chosen_key(k))
        {
            if let Some(tools) = self.tools.upgrade() {
                if let Ok(place) = crate::chosen_me::place_of(self.on, &tools).await {
                    if let Some(user) = crate::chosen_me::chosen_of(&identity, self.on, &place) {
                        return Ok(ToolResult::success(
                            json!({ "user": user, "why": null, "source": "chosen" }),
                            ms(),
                        ));
                    }
                }
            }
        }
        let (email, source) = match crate::oidc::verified_email_state(&identity, chrono::Utc::now())
        {
            VerifiedEmail::Fresh(email, source) => (email, source),
            VerifiedEmail::Stale => return Ok(ToolResult::success(none("stale"), ms())),
            VerifiedEmail::None => return Ok(ToolResult::success(none("no_verified_email"), ms())),
        };
        let (lookup, args) = self.on.lookup(&email);
        let Some(tool) = self.tools.upgrade().and_then(|t| t.get(lookup)) else {
            return Ok(ToolResult::failure(
                format!("This agent has no {lookup} to look you up with."),
                ms(),
            ));
        };
        let answer = tool.execute(args).await?;
        if !answer.success {
            return Ok(ToolResult::failure(
                answer.error.unwrap_or_else(|| format!("{lookup} failed")),
                ms(),
            ));
        }
        let mut found = pick(self.on, &answer.output, &email);
        // Whose word the email is: a sign-in's, or Slack's.
        found["source"] = json!(source.name());
        Ok(ToolResult::success(found, ms()))
    }
}

#[cfg(test)]
#[path = "by_my_email_tests.rs"]
mod tests;
