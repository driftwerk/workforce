//! Who a person says they are on a shared Jira or Linear key: chosen from
//! the members there, kept on the hub per person per Jira site or Linear
//! workspace, and used before a verified email ([`crate::by_my_email`]).
//!
//! No proof is asked for: the match only decides whom an honest Desk names
//! as the person (Mine, "Assigned to me", Take it). It grants nothing a
//! device token on the shared key can't already do (assign anyone).
//!
//! - `identity.jira_users_named` / `identity.linear_members_named`: the
//!   active members whose name contains what was typed, as `{id, name,
//!   avatar}`; never an email, never the whole directory.
//! - `identity.jira_choose_me` / `identity.linear_choose_me`: keeps (or
//!   clears) the caller's own choice. There's no way to name another
//!   person: it is always the caller's, and an identity edit never writes
//!   it ([`crate::verified_email::keep_stored_verified`]).

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
use workforce_identity::types::{Identity, IdentityStatus};

use crate::by_my_email::On;

/// Where a choice is kept in `linked_ids`: `chosen.jira:<site>`.
const CHOSEN_PREFIX: &str = "chosen.";

/// The fewest characters a name search takes: no listing everyone.
pub const MIN_NAME: usize = 2;

/// The most members a name search returns.
pub const MAX_MEMBERS: usize = 20;

/// Whether `key` is a choice of who someone is (`chosen.<system>:<where>`).
pub fn is_chosen_key(key: &str) -> bool {
    key.starts_with(CHOSEN_PREFIX)
}

fn system(on: On) -> &'static str {
    match on {
        On::Jira => "jira",
        On::Linear => "linear",
    }
}

/// The key `on`'s choice is kept under, for the workspace `place`.
pub fn chosen_key(on: On, place: &str) -> String {
    format!(
        "{CHOSEN_PREFIX}{}:{}",
        system(on),
        place.trim().to_ascii_lowercase()
    )
}

/// Who `identity` chose to be on `on` in `place`: `{id, name}`.
pub fn chosen_of(identity: &Identity, on: On, place: &str) -> Option<Value> {
    let kept: Value =
        serde_json::from_str(identity.linked_ids.get(&chosen_key(on, place))?).ok()?;
    let id = kept["id"].as_str().filter(|i| !i.trim().is_empty())?;
    Some(json!({ "id": id, "name": kept["name"].as_str().unwrap_or(id) }))
}

/// Which Jira site or Linear workspace the agent's key is in, as its
/// identity tool says; "default" when it doesn't say (an older adapter).
pub async fn place_of(on: On, tools: &ToolRegistry) -> Result<String, String> {
    let (tool, field) = match on {
        On::Jira => ("jira.myself", "site"),
        On::Linear => ("linear.viewer", "workspace"),
    };
    let Some(tool) = tools.get(tool) else {
        return Err(format!("This agent has no {} to say where it is.", tool));
    };
    let answer = tool
        .execute(json!({}))
        .await
        .map_err(|e| format!("{e:#}"))?;
    if !answer.success {
        return Err(answer.error.unwrap_or_else(|| "it failed".into()));
    }
    Ok(answer.output[field]
        .as_str()
        .map(|p| {
            p.trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "default".into()))
}

/// The caller, an active identity, or why not.
fn caller(
    store: &RwLock<Option<Arc<dyn IdentityStore>>>,
) -> Result<(Arc<dyn IdentityStore>, Identity), &'static str> {
    let user = crate::user_config::current_identity_id()
        .ok_or("No one is asking: there's no user in this call.")?;
    let store = store.read().clone().ok_or("This hub keeps no people.")?;
    let identity = Uuid::parse_str(&user)
        .ok()
        .and_then(|id| store.get_by_id(id).ok().flatten())
        .ok_or("The hub doesn't know who's asking.")?;
    if !matches!(
        identity.status,
        IdentityStatus::Active | IdentityStatus::AutoProvisioned
    ) {
        return Err("Only an active person chooses who they are.");
    }
    Ok((store, identity))
}

/// The members in an adapter's `answer` whose name contains `needle`
/// (any case), active (on Jira, atlassian accounts), as `{id, name,
/// avatar}`: no email, whatever the adapter gave.
pub fn members_named(on: On, answer: &Value, needle: &str) -> Vec<Value> {
    let needle = needle.trim().to_lowercase();
    let (list, id, names, avatar): (&str, &str, &[&str], &str) = match on {
        On::Linear => ("members", "id", &["displayName", "name"], "avatarUrl"),
        On::Jira => ("users", "account_id", &["display_name"], "avatar"),
    };
    answer[list]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|u| u["active"].as_bool() == Some(true))
        .filter(|u| on != On::Jira || u["account_type"].as_str() == Some("atlassian"))
        .filter_map(|u| {
            let id = u[id].as_str().filter(|i| !i.trim().is_empty())?;
            let shown: Vec<&str> = names
                .iter()
                .filter_map(|n| u[*n].as_str())
                .filter(|n| !n.trim().is_empty())
                .collect();
            let name = *shown.first()?;
            shown
                .iter()
                .any(|n| n.to_lowercase().contains(&needle))
                .then(|| json!({ "id": id, "name": name, "avatar": u[avatar].as_str().filter(|a| !is_gravatar(a)) }))
        })
        .take(MAX_MEMBERS)
        .collect()
}

/// Whether `avatar` is a Gravatar picture, whose address is a hash of the
/// person's email (Jira's default avatars are): not shown.
fn is_gravatar(avatar: &str) -> bool {
    url::Url::parse(avatar)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| h == "gravatar.com" || h.ends_with(".gravatar.com"))
}

/// `identity.jira_users_named` / `identity.linear_members_named`.
pub struct MembersNamed {
    on: On,
    tools: Weak<ToolRegistry>,
    identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

impl MembersNamed {
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

#[async_trait]
impl Tool for MembersNamed {
    fn name(&self) -> &str {
        match self.on {
            On::Jira => "identity.jira_users_named",
            On::Linear => "identity.linear_members_named",
        }
    }

    fn description(&self) -> &str {
        match self.on {
            On::Jira => "The active Jira users on this agent's key whose name contains `name` (at least 2 characters), as {members: [{id, name, avatar}]}, at most 20: to choose who you are there with identity.jira_choose_me. Never emails.",
            On::Linear => "The active Linear members on this agent's key whose name contains `name` (at least 2 characters), as {members: [{id, name, avatar}]}, at most 20: to choose who you are there with identity.linear_choose_me. Never emails.",
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Part of their name, at least 2 characters" } },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        if let Err(why) = caller(&self.identity_store) {
            return Ok(ToolResult::failure(why, ms()));
        }
        let needle = input["name"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if needle.chars().count() < MIN_NAME {
            return Ok(ToolResult::failure(
                format!("Type at least {MIN_NAME} letters of their name."),
                ms(),
            ));
        }
        let (lookup, args) = match self.on {
            On::Jira => (
                "jira.search_users",
                json!({ "query": needle, "max_results": 50 }),
            ),
            On::Linear => (
                "linear.members_list",
                json!({ "name": needle, "limit": 50 }),
            ),
        };
        let Some(tool) = self.tools.upgrade().and_then(|t| t.get(lookup)) else {
            return Ok(ToolResult::failure(
                format!("This agent has no {lookup} to look in."),
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
        Ok(ToolResult::success(
            json!({ "members": members_named(self.on, &answer.output, &needle) }),
            ms(),
        ))
    }
}

/// `identity.jira_choose_me` / `identity.linear_choose_me`.
pub struct ChooseMe {
    on: On,
    tools: Weak<ToolRegistry>,
    identity_store: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

impl ChooseMe {
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

/// Whether `id` can be someone on `on`: a Jira accountId, a Linear id.
fn fits(on: On, id: &str) -> bool {
    match on {
        On::Jira => {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_'))
        }
        On::Linear => Uuid::parse_str(id).is_ok(),
    }
}

#[async_trait]
impl Tool for ChooseMe {
    fn name(&self) -> &str {
        match self.on {
            On::Jira => "identity.jira_choose_me",
            On::Linear => "identity.linear_choose_me",
        }
    }

    fn description(&self) -> &str {
        match self.on {
            On::Jira => "Say who you are on this agent's Jira site, when its key is shared: {id, name} of a user from identity.jira_users_named (\"I'm K. Hendrickse on Jira\"), or {clear: true} to go back to your verified email. Only ever your own; boards then go by it for Mine, Assigned to me and Take it.",
            On::Linear => "Say who you are in this agent's Linear workspace, when its key is shared: {id, name} of a member from identity.linear_members_named (\"I'm K. Hendrickse on Linear\"), or {clear: true} to go back to your verified email. Only ever your own; boards then go by it for Mine, Assigned to me and Take it.",
        }
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Their id there (a Jira accountId, a Linear user id)" },
                "name": { "type": "string", "description": "Their name there, as shown" },
                "clear": { "type": "boolean", "description": "Forget your choice" },
            },
            "additionalProperties": false,
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let ms = || start.elapsed().as_millis() as u64;
        let (store, identity) = match caller(&self.identity_store) {
            Ok(c) => c,
            Err(why) => return Ok(ToolResult::failure(why, ms())),
        };
        let clear = input["clear"].as_bool() == Some(true);
        let id = input["id"]
            .as_str()
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if !clear && !fits(self.on, &id) {
            return Ok(ToolResult::failure(
                match self.on {
                    On::Jira => {
                        "That isn't a Jira accountId: choose from identity.jira_users_named."
                    }
                    On::Linear => {
                        "That isn't a Linear user id: choose from identity.linear_members_named."
                    }
                },
                ms(),
            ));
        }
        let Some(tools) = self.tools.upgrade() else {
            return Ok(ToolResult::failure("This agent is stopping.", ms()));
        };
        let place = match place_of(self.on, &tools).await {
            Ok(p) => p,
            Err(e) => return Ok(ToolResult::failure(e, ms())),
        };
        let key = chosen_key(self.on, &place);
        // Read again, and write only the caller's own.
        let mut me = match store.get_by_id(identity.id) {
            Ok(Some(me)) => me,
            _ => {
                return Ok(ToolResult::failure(
                    "The hub doesn't know who's asking.",
                    ms(),
                ))
            }
        };
        let out = if clear {
            me.linked_ids.remove(&key);
            json!({ "cleared": true, "place": place })
        } else {
            let name: String = input["name"]
                .as_str()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .unwrap_or(&id)
                .chars()
                .take(80)
                .collect();
            let chosen = json!({ "id": id, "name": name });
            me.linked_ids.insert(key, chosen.to_string());
            json!({ "chosen": chosen, "place": place })
        };
        me.updated_at = chrono::Utc::now();
        if let Err(e) = store.update(&me) {
            return Ok(ToolResult::failure(
                format!("Keeping it failed: {e:#}"),
                ms(),
            ));
        }
        Ok(ToolResult::success(out, ms()))
    }
}

#[cfg(test)]
#[path = "chosen_me_tests.rs"]
mod tests;
