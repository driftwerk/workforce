//! Per-user preference layer.
//!
//! Where `lessons` captures failure-shaped gotchas, preferences capture durable
//! user-stated choices — verbosity, format, tone, language, workflow. They live
//! at `MemoryLayer::Semantic` with `MemorySource::Preference`, are strictly
//! per-user (`identity_id`), and — unlike lessons — are *always* injected for
//! the user they belong to rather than recall-score-gated, so the agent
//! reliably applies them. A new preference in the same `category` replaces the
//! prior one, so the always-on block can never accumulate contradictory
//! standing instructions ("be terse" *and* "be detailed").

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{ensure, Result};
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySource};

use crate::lessons::DATE_TAG_PREFIX;
use crate::turn_extraction::{
    remember_with_dedup, DedupConfig, WriteOutcome, LESSON_NEAR_DUPLICATE_SIMILARITY,
};

pub const PREFERENCE_SOURCE_TAG: &str = "source:preference";
pub const PREFERENCE_CATEGORY_PREFIX: &str = "pref-category:";

/// Cap on preferences rendered into the prompt per user. Preferences are not
/// score-gated, so this bounds the block; importance/recency order the drop.
pub const PREFERENCE_INJECT_LIMIT: usize = 30;

/// Dedup window for preferences — long, because a preference restated months
/// apart is the same standing instruction, not a new event.
const PREFERENCE_DEDUP_WINDOW: chrono::Duration = chrono::Duration::days(365);

/// Importance assigned to a stored preference. High so the always-on block
/// orders preferences above incidental noise when the cap bites.
const PREFERENCE_IMPORTANCE: f32 = 0.9;

/// Outcome of [`remember_preference`].
#[derive(Debug, Clone)]
pub struct PreferenceOutcome {
    pub id: String,
    /// Human sentence the tool returns for the agent to relay.
    pub confirmation: String,
    /// How many same-category preferences this write replaced.
    pub replaced: usize,
}

fn preference_dedup_config() -> DedupConfig {
    DedupConfig {
        layer: MemoryLayer::Semantic,
        source: Some(MemorySource::Preference),
        window: PREFERENCE_DEDUP_WINDOW,
        near_duplicate_similarity: LESSON_NEAR_DUPLICATE_SIMILARITY,
    }
}

/// Lowercase, whitespace-collapsed-to-`-` category handle, or `None` when the
/// input is absent / blank.
fn normalise_category(category: Option<&str>) -> Option<String> {
    category.map(str::trim).filter(|c| !c.is_empty()).map(|c| {
        c.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-")
    })
}

fn category_of(mem: &Memory) -> Option<&str> {
    mem.tags
        .iter()
        .find_map(|t| t.strip_prefix(PREFERENCE_CATEGORY_PREFIX))
}

fn build_preference(
    agent_id: &AgentId,
    identity_id: &str,
    preference: &str,
    category: Option<&str>,
) -> Memory {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut tags = vec![PREFERENCE_SOURCE_TAG.to_string()];
    if let Some(cat) = category {
        tags.push(format!("{PREFERENCE_CATEGORY_PREFIX}{cat}"));
    }
    tags.push(format!("{DATE_TAG_PREFIX}{date}"));
    Memory::new(preference, MemoryLayer::Semantic, agent_id.clone())
        .with_source(MemorySource::Preference)
        .with_importance(PREFERENCE_IMPORTANCE)
        .with_identity_id(identity_id)
        .with_tags(tags)
}

/// All non-deleted preferences for `(agent, identity)`, newest first.
pub async fn list_preferences(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    identity_id: &str,
) -> Result<Vec<Memory>> {
    let filter = MemoryFilter {
        owner: Some(agent_id.clone()),
        source: Some(MemorySource::Preference),
        identity_id: Some(identity_id.to_string()),
        limit: Some(PREFERENCE_INJECT_LIMIT.saturating_mul(4).max(1)),
        ..Default::default()
    };
    let mut prefs = store.list(filter).await?;
    // The id breaks ties: the backend list is unordered and this renders
    // into the cached system prompt.
    prefs.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    Ok(prefs)
}

/// The preferences to inject this turn: newest-first, capped.
pub async fn recent_preferences(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    identity_id: &str,
) -> Result<Vec<Memory>> {
    let mut prefs = list_preferences(store, agent_id, identity_id).await?;
    prefs.truncate(PREFERENCE_INJECT_LIMIT);
    Ok(prefs)
}

/// Persist a durable user preference. A new preference in the same `category`
/// replaces existing same-category ones (soft-deleted, kept for audit);
/// category-less preferences merge against near-duplicates so re-stating the
/// same preference does not pile up.
pub async fn remember_preference(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    identity_id: &str,
    preference: &str,
    category: Option<&str>,
) -> Result<PreferenceOutcome> {
    let preference = preference.trim();
    ensure!(!preference.is_empty(), "preference is empty");
    let category = normalise_category(category);

    let mut replaced = 0usize;
    if let Some(ref cat) = category {
        for mem in list_preferences(store, agent_id, identity_id)
            .await?
            .into_iter()
            .filter(|m| category_of(m) == Some(cat.as_str()))
        {
            store.forget(&mem.id).await?;
            replaced += 1;
        }
    }

    let memory = build_preference(agent_id, identity_id, preference, category.as_deref());
    let new_id = memory.id.0.clone();
    let id = if category.is_some() {
        store.remember(memory).await?;
        new_id
    } else {
        match remember_with_dedup(store, memory, preference_dedup_config()).await? {
            WriteOutcome::Inserted => new_id,
            WriteOutcome::MergedInto { id, .. } => id.0,
        }
    };

    let confirmation = if replaced > 0 {
        format!("Updated your saved preference: \"{preference}\".")
    } else {
        format!("Saved your preference: \"{preference}\" — I'll apply it from now on.")
    };
    Ok(PreferenceOutcome {
        id,
        confirmation,
        replaced,
    })
}

/// Render the always-on preferences block. Empty string when there are none —
/// never emit the header without content. The "say so when you apply one"
/// instruction lives once in the cached legibility section, not here.
pub fn format_preferences_for_prompt(prefs: &[Memory]) -> String {
    if prefs.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## Your Preferences\n\
         _Standing preferences this user has set. Apply them._\n",
    );
    for p in prefs {
        match category_of(p) {
            Some(cat) => out.push_str(&format!("- ({cat}) {}\n", p.content)),
            None => out.push_str(&format!("- {}\n", p.content)),
        }
    }
    out
}

/// Remove preferences matching `category` (exact) or `content` (case-insensitive
/// substring); at least one criterion is required. Returns the removed
/// preference texts so a destructive change can be confirmed back to the user.
pub async fn forget_preferences(
    store: &dyn MemoryStore,
    agent_id: &AgentId,
    identity_id: &str,
    category: Option<&str>,
    content: Option<&str>,
) -> Result<Vec<String>> {
    let category = normalise_category(category);
    let content = content
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_lowercase);
    ensure!(
        category.is_some() || content.is_some(),
        "forget needs a category or preference text to match"
    );
    let mut removed = Vec::new();
    for mem in list_preferences(store, agent_id, identity_id).await? {
        let by_category = category
            .as_deref()
            .is_some_and(|c| category_of(&mem) == Some(c));
        let by_content = content
            .as_deref()
            .is_some_and(|c| mem.content.to_lowercase().contains(c));
        if by_category || by_content {
            store.forget(&mem.id).await?;
            removed.push(mem.content);
        }
    }
    Ok(removed)
}

// ─── Tool ───────────────────────────────────────────────────────────────────

/// Agent tool that manages durable user preferences synchronously, so a save
/// confirmation can be relayed in the same turn the preference is stated.
pub struct PreferenceTool {
    store: Arc<dyn MemoryStore>,
    agent_id: AgentId,
}

impl PreferenceTool {
    pub fn new(store: Arc<dyn MemoryStore>, agent_id: AgentId) -> Self {
        Self { store, agent_id }
    }
}

#[derive(Deserialize)]
struct PreferenceInput {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    preference: Option<String>,
    #[serde(default)]
    category: Option<String>,
}

#[async_trait]
impl Tool for PreferenceTool {
    fn name(&self) -> &str {
        "remember_preference"
    }

    fn description(&self) -> &str {
        "Manage the user's durable preferences (verbosity, format, tone, language, workflow) — \
         standing choices applied in every future reply. action='save' (default) records one the \
         moment the user states it ('keep answers short', 'always show the SQL', 'use British \
         spelling'); a new preference in the same `category` replaces the previous one. \
         action='list' returns the user's current preferences; action='forget' removes those \
         matching `category` or `preference` text. Returns a confirmation to relay to the user."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["save", "list", "forget"],
                    "description": "save (default) records a preference; list returns the user's current preferences; forget removes those matching category or preference text."
                },
                "preference": {
                    "type": "string",
                    "description": "For save: the standing preference phrased as an instruction ('Keep answers concise and skip preamble'). For forget: text to match against existing preferences."
                },
                "category": {
                    "type": "string",
                    "description": "Optional short bucket (verbosity, format, tone, language, workflow). On save a new preference in the same category replaces the previous one; on forget it removes that category."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let input: PreferenceInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };
        let Some(identity_id) = crate::user_config::current_identity_id() else {
            return Ok(ToolResult::failure(
                "remember_preference requires a user identity in scope; refusing to run unscoped",
                elapsed(),
            ));
        };
        let identity_id = identity_id.as_str();
        let store = self.store.as_ref();

        match input.action.as_deref().unwrap_or("save") {
            "save" => {
                let preference = input
                    .preference
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default();
                if preference.is_empty() {
                    return Ok(ToolResult::failure(
                        "Missing 'preference' — what should I remember?",
                        elapsed(),
                    ));
                }
                match remember_preference(
                    store,
                    &self.agent_id,
                    identity_id,
                    preference,
                    input.category.as_deref(),
                )
                .await
                {
                    Ok(outcome) => Ok(ToolResult::success(
                        serde_json::json!({
                            "preference_id": outcome.id,
                            "preference": preference,
                            "replaced": outcome.replaced,
                            "status": if outcome.replaced > 0 { "updated" } else { "saved" },
                            "confirmation": outcome.confirmation,
                        }),
                        elapsed(),
                    )),
                    Err(e) => Ok(ToolResult::failure(
                        format!("Failed to save preference: {e}"),
                        elapsed(),
                    )),
                }
            }
            "list" => match list_preferences(store, &self.agent_id, identity_id).await {
                Ok(prefs) => {
                    let items: Vec<serde_json::Value> = prefs
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "preference": p.content,
                                "category": category_of(p),
                            })
                        })
                        .collect();
                    Ok(ToolResult::success(
                        serde_json::json!({ "count": items.len(), "preferences": items }),
                        elapsed(),
                    ))
                }
                Err(e) => Ok(ToolResult::failure(
                    format!("Failed to list preferences: {e}"),
                    elapsed(),
                )),
            },
            "forget" => match forget_preferences(
                store,
                &self.agent_id,
                identity_id,
                input.category.as_deref(),
                input.preference.as_deref(),
            )
            .await
            {
                Ok(removed) => Ok(ToolResult::success(
                    serde_json::json!({
                        "removed_count": removed.len(),
                        "removed": removed,
                        "status": if removed.is_empty() { "no_match" } else { "forgotten" },
                    }),
                    elapsed(),
                )),
                Err(e) => Ok(ToolResult::failure(
                    format!("Failed to forget preference: {e}"),
                    elapsed(),
                )),
            },
            other => Ok(ToolResult::failure(
                format!("Unknown action '{other}'. Use: save, list, forget"),
                elapsed(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_memory::in_memory::InMemoryStore;

    const USER: &str = "user-1";

    fn store_agent() -> (Arc<dyn MemoryStore>, AgentId) {
        (Arc::new(InMemoryStore::new()), AgentId::new("agent"))
    }

    #[test]
    fn normalise_category_kebabs_and_blanks() {
        assert_eq!(
            normalise_category(Some(" Verbosity ")),
            Some("verbosity".into())
        );
        assert_eq!(
            normalise_category(Some("UI format")),
            Some("ui-format".into())
        );
        assert_eq!(normalise_category(Some("   ")), None);
        assert_eq!(normalise_category(None), None);
    }

    #[tokio::test]
    async fn same_category_preference_replaces_prior() {
        let (store, agent) = store_agent();
        let first =
            remember_preference(store.as_ref(), &agent, USER, "Be terse", Some("verbosity"))
                .await
                .unwrap();
        assert_eq!(first.replaced, 0);

        let second = remember_preference(
            store.as_ref(),
            &agent,
            USER,
            "Be detailed and thorough",
            Some("verbosity"),
        )
        .await
        .unwrap();
        assert_eq!(second.replaced, 1);

        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 1, "same-category replace must not accumulate");
        assert_eq!(live[0].content, "Be detailed and thorough");
    }

    #[tokio::test]
    async fn categoryless_restatement_dedupes() {
        let (store, agent) = store_agent();
        remember_preference(store.as_ref(), &agent, USER, "Always show the SQL", None)
            .await
            .unwrap();
        remember_preference(store.as_ref(), &agent, USER, "Always show the SQL", None)
            .await
            .unwrap();
        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 1, "near-duplicate restatement must merge");
    }

    #[tokio::test]
    async fn distinct_categories_coexist() {
        let (store, agent) = store_agent();
        remember_preference(store.as_ref(), &agent, USER, "Be terse", Some("verbosity"))
            .await
            .unwrap();
        remember_preference(
            store.as_ref(),
            &agent,
            USER,
            "Use British spelling",
            Some("language"),
        )
        .await
        .unwrap();
        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 2);
    }

    #[tokio::test]
    async fn preferences_are_per_user_isolated() {
        let (store, agent) = store_agent();
        remember_preference(store.as_ref(), &agent, USER, "Be terse", Some("verbosity"))
            .await
            .unwrap();
        let other = recent_preferences(store.as_ref(), &agent, "user-2")
            .await
            .unwrap();
        assert!(
            other.is_empty(),
            "one user's preferences must not leak to another"
        );
    }

    #[tokio::test]
    async fn empty_preference_is_rejected() {
        let (store, agent) = store_agent();
        let err = remember_preference(store.as_ref(), &agent, USER, "   ", None).await;
        assert!(err.is_err());
    }

    #[test]
    fn format_block_empty_when_no_prefs() {
        assert!(format_preferences_for_prompt(&[]).is_empty());
    }

    #[test]
    fn format_block_renders_category_and_plain() {
        let agent = AgentId::new("agent");
        let with_cat = build_preference(&agent, USER, "Be terse", Some("verbosity"));
        let plain = build_preference(&agent, USER, "Always show the SQL", None);
        let block = format_preferences_for_prompt(&[with_cat, plain]);
        assert!(block.contains("## Your Preferences"));
        assert!(block.contains("(verbosity) Be terse"));
        assert!(block.contains("- Always show the SQL"));
    }

    #[tokio::test]
    async fn tool_rejects_when_no_identity_in_scope() {
        let (store, agent) = store_agent();
        let tool = PreferenceTool::new(store, agent);
        let result = tool
            .execute(serde_json::json!({ "preference": "Be terse" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("requires a user identity"));
    }

    #[tokio::test]
    async fn tool_saves_under_identity_scope() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let (store, agent) = store_agent();
        let tool = PreferenceTool::new(Arc::clone(&store), agent.clone());
        CURRENT_USER_ID
            .scope(USER.to_string(), async {
                let result = tool
                    .execute(serde_json::json!({
                        "preference": "Keep answers concise",
                        "category": "verbosity"
                    }))
                    .await
                    .unwrap();
                assert!(result.success);
                assert_eq!(result.output["status"], "saved");
            })
            .await;
        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
    }

    #[tokio::test]
    async fn forget_by_category_removes_only_that_category() {
        let (store, agent) = store_agent();
        remember_preference(store.as_ref(), &agent, USER, "Be terse", Some("verbosity"))
            .await
            .unwrap();
        remember_preference(
            store.as_ref(),
            &agent,
            USER,
            "Use British spelling",
            Some("language"),
        )
        .await
        .unwrap();
        let removed = forget_preferences(store.as_ref(), &agent, USER, Some("verbosity"), None)
            .await
            .unwrap();
        assert_eq!(removed, vec!["Be terse".to_string()]);
        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].content, "Use British spelling");
    }

    #[tokio::test]
    async fn forget_by_content_substring_removes_matching() {
        let (store, agent) = store_agent();
        remember_preference(store.as_ref(), &agent, USER, "Always show the SQL", None)
            .await
            .unwrap();
        remember_preference(store.as_ref(), &agent, USER, "Keep answers concise", None)
            .await
            .unwrap();
        let removed = forget_preferences(store.as_ref(), &agent, USER, None, Some("sql"))
            .await
            .unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].to_lowercase().contains("sql"));
        let live = recent_preferences(store.as_ref(), &agent, USER)
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
    }

    #[tokio::test]
    async fn forget_requires_a_criterion() {
        let (store, agent) = store_agent();
        let err = forget_preferences(store.as_ref(), &agent, USER, None, None).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn tool_list_then_forget_round_trip() {
        use workforce_agent::runner::CURRENT_USER_ID;
        let (store, agent) = store_agent();
        let tool = PreferenceTool::new(Arc::clone(&store), agent.clone());
        CURRENT_USER_ID
            .scope(USER.to_string(), async {
                tool.execute(serde_json::json!({
                    "preference": "Be terse",
                    "category": "verbosity"
                }))
                .await
                .unwrap();

                let listed = tool
                    .execute(serde_json::json!({ "action": "list" }))
                    .await
                    .unwrap();
                assert!(listed.success);
                assert_eq!(listed.output["count"], 1);

                let forgotten = tool
                    .execute(serde_json::json!({ "action": "forget", "category": "verbosity" }))
                    .await
                    .unwrap();
                assert!(forgotten.success);
                assert_eq!(forgotten.output["removed_count"], 1);

                let empty = tool
                    .execute(serde_json::json!({ "action": "list" }))
                    .await
                    .unwrap();
                assert_eq!(empty.output["count"], 0);
            })
            .await;
    }
}
