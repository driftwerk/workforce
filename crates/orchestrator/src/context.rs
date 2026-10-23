//! Multi-agent context management for team conversations.
//!
//! Provides per-agent context views that include recent shared history,
//! summarised older turns, the agent's own contributions (always full),
//! and relevant artifacts from the shared workspace.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use workforce_core::types::AgentId;

use crate::workspace::{Artifact, ArtifactType, SharedWorkspace};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single turn in the shared team conversation.
#[derive(Debug, Clone)]
pub struct ConversationTurn {
    pub speaker: AgentId,
    pub role: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub token_estimate: u32,
}

/// Configuration for how context views are built.
#[derive(Debug, Clone)]
pub struct ContextViewConfig {
    /// Number of most-recent turns to include in full.
    pub recent_full_turns: usize,
    /// Maximum tokens for the summarised older-history section.
    pub older_summary_max_tokens: u32,
    /// Whether to inline small artifacts directly.
    pub inline_small_artifacts: bool,
    /// Artifacts smaller than this (in estimated tokens) get inlined.
    pub artifact_inline_threshold: u32,
}

impl Default for ContextViewConfig {
    fn default() -> Self {
        Self {
            recent_full_turns: 10,
            older_summary_max_tokens: 2000,
            inline_small_artifacts: true,
            artifact_inline_threshold: 500,
        }
    }
}

/// Artifact context entry included in a view.
#[derive(Debug, Clone)]
pub struct ArtifactContext {
    pub artifact_id: String,
    pub title: String,
    /// Full content (if inlined) or a truncated summary.
    pub content: String,
    /// Relevance score (0.0–1.0).
    pub relevance: f64,
    /// Whether this is a summary rather than the full artifact.
    pub is_summary: bool,
}

/// An agent-specific view of the team conversation context.
#[derive(Debug, Clone)]
pub struct ContextView {
    pub agent_id: AgentId,
    /// Recent turns included in full.
    pub recent_turns: Vec<ConversationTurn>,
    /// Simple extractive summary of older turns.
    pub older_summary: String,
    /// All of this agent's own contributions (always full).
    pub own_contributions: Vec<ConversationTurn>,
    /// Relevant artifacts from the shared workspace.
    pub artifact_context: Vec<ArtifactContext>,
    /// Estimated total tokens for this view.
    pub estimated_tokens: u32,
}

// ---------------------------------------------------------------------------
// TeamContextManager
// ---------------------------------------------------------------------------

/// Manages shared conversation history and builds per-agent context views.
pub struct TeamContextManager {
    team_id: String,
    session_id: String,
    history: Vec<ConversationTurn>,
    /// Generation-keyed cache: (generation, ContextView).
    view_cache: HashMap<AgentId, (u64, ContextView)>,
    generation: u64,
    view_config: ContextViewConfig,
    workspace: Option<SharedWorkspace>,
}

impl TeamContextManager {
    /// Create a new context manager for a team session.
    pub fn new(
        team_id: impl Into<String>,
        session_id: impl Into<String>,
        workspace: Option<SharedWorkspace>,
        view_config: ContextViewConfig,
    ) -> Self {
        Self {
            team_id: team_id.into(),
            session_id: session_id.into(),
            history: Vec::new(),
            view_cache: HashMap::new(),
            generation: 0,
            view_config,
            workspace,
        }
    }

    /// Record a new turn in the shared conversation history.
    pub fn record_turn(
        &mut self,
        speaker: AgentId,
        role: impl Into<String>,
        content: impl Into<String>,
    ) {
        let content = content.into();
        let token_estimate = estimate_tokens(&content);
        self.history.push(ConversationTurn {
            speaker,
            role: role.into(),
            content,
            timestamp: Utc::now(),
            token_estimate,
        });
        self.generation += 1;
    }

    /// Build a context view for a specific agent.
    ///
    /// Results are cached per generation — repeated calls with the same
    /// generation return the cached view.
    pub fn context_for(
        &mut self,
        agent_id: &AgentId,
        agent_role: &str,
        agent_expertise: &[String],
        max_tokens: u32,
    ) -> ContextView {
        // Check cache
        if let Some((gen, cached)) = self.view_cache.get(agent_id) {
            if *gen == self.generation {
                return cached.clone();
            }
        }

        let view = self.build_view(agent_id, agent_role, agent_expertise, max_tokens);
        self.view_cache
            .insert(agent_id.clone(), (self.generation, view.clone()));
        view
    }

    /// Number of turns recorded so far.
    pub fn turn_count(&self) -> usize {
        self.history.len()
    }

    /// Full conversation history.
    pub fn history(&self) -> &[ConversationTurn] {
        &self.history
    }

    /// The team id.
    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    /// The session id.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn build_view(
        &self,
        agent_id: &AgentId,
        agent_role: &str,
        agent_expertise: &[String],
        max_tokens: u32,
    ) -> ContextView {
        let n = self.history.len();
        let split = n.saturating_sub(self.view_config.recent_full_turns);

        // Recent turns (full)
        let recent_turns: Vec<ConversationTurn> = self.history[split..].to_vec();

        // Older turns → extractive summary (first sentence of each)
        let older_summary = self.summarise_older(&self.history[..split]);

        // Own contributions — always full, from entire history
        let own_contributions: Vec<ConversationTurn> = self
            .history
            .iter()
            .filter(|t| t.speaker == *agent_id)
            .cloned()
            .collect();

        // Artifacts from workspace
        let artifact_context = self.score_artifacts(agent_role, agent_expertise);

        // Token estimate
        let recent_tokens: u32 = recent_turns.iter().map(|t| t.token_estimate).sum();
        let summary_tokens = estimate_tokens(&older_summary);
        let own_tokens: u32 = own_contributions.iter().map(|t| t.token_estimate).sum();
        let artifact_tokens: u32 = artifact_context
            .iter()
            .map(|a| estimate_tokens(&a.content))
            .sum();

        let mut estimated_tokens = recent_tokens + summary_tokens + own_tokens + artifact_tokens;

        // If we're over budget, trim the older summary
        let mut final_summary = older_summary;
        if estimated_tokens > max_tokens && summary_tokens > 0 {
            let budget = max_tokens.saturating_sub(estimated_tokens - summary_tokens);
            final_summary = truncate_to_tokens(&final_summary, budget);
            estimated_tokens =
                recent_tokens + estimate_tokens(&final_summary) + own_tokens + artifact_tokens;
        }

        ContextView {
            agent_id: agent_id.clone(),
            recent_turns,
            older_summary: final_summary,
            own_contributions,
            artifact_context,
            estimated_tokens,
        }
    }

    /// Extractive summary: first sentence of each older turn, capped by config.
    fn summarise_older(&self, turns: &[ConversationTurn]) -> String {
        if turns.is_empty() {
            return String::new();
        }

        let max_tokens = self.view_config.older_summary_max_tokens;
        let mut parts = Vec::new();
        let mut total_tokens: u32 = 0;

        for turn in turns {
            let first_sentence = first_sentence_of(&turn.content);
            let line = format!("[{}] {}", turn.speaker.as_str(), first_sentence);
            let line_tokens = estimate_tokens(&line);
            if total_tokens + line_tokens > max_tokens {
                break;
            }
            total_tokens += line_tokens;
            parts.push(line);
        }

        parts.join("\n")
    }

    /// Score and collect artifacts from the workspace, sorted by relevance.
    fn score_artifacts(
        &self,
        agent_role: &str,
        agent_expertise: &[String],
    ) -> Vec<ArtifactContext> {
        let workspace = match &self.workspace {
            Some(ws) => ws,
            None => return Vec::new(),
        };

        let artifacts = match workspace.list_artifacts() {
            Ok(a) => a,
            Err(_) => return Vec::new(),
        };

        let mut scored: Vec<(f64, ArtifactContext)> = artifacts
            .into_iter()
            .map(|a| {
                let relevance = artifact_relevance(&a, agent_role, agent_expertise);
                let tokens = estimate_tokens(&a.content);
                let (content, is_summary) = if self.view_config.inline_small_artifacts
                    && tokens <= self.view_config.artifact_inline_threshold
                {
                    (a.content.clone(), false)
                } else {
                    let summary =
                        truncate_to_tokens(&a.content, self.view_config.artifact_inline_threshold);
                    (summary, true)
                };

                (
                    relevance,
                    ArtifactContext {
                        artifact_id: a.id.clone(),
                        title: a.title.clone(),
                        content,
                        relevance,
                        is_summary,
                    },
                )
            })
            .filter(|(rel, _)| *rel > 0.0)
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(_, ctx)| ctx).collect()
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Estimate token count from content length (matches existing pattern).
fn estimate_tokens(content: &str) -> u32 {
    (content.len() / 4) as u32
}

/// Extract the first sentence from a string.
fn first_sentence_of(s: &str) -> &str {
    // Find first sentence-ending punctuation followed by whitespace or end
    for (i, c) in s.char_indices() {
        if (c == '.' || c == '!' || c == '?')
            && s[i + c.len_utf8()..].starts_with(|ch: char| ch.is_whitespace())
        {
            return &s[..=i];
        }
    }
    // No sentence-ender found; take first 200 chars
    if s.len() > 200 {
        // Find a safe char boundary
        let end = s.char_indices().nth(200).map(|(i, _)| i).unwrap_or(s.len());
        &s[..end]
    } else {
        s
    }
}

/// Truncate content to approximately `max_tokens` tokens.
fn truncate_to_tokens(content: &str, max_tokens: u32) -> String {
    let max_chars = (max_tokens as usize) * 4;
    if content.len() <= max_chars {
        return content.to_string();
    }
    // Find safe char boundary
    let end = content
        .char_indices()
        .take_while(|(i, _)| *i < max_chars)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}...", &content[..end])
}

/// Score an artifact's relevance to a given agent role and expertise.
fn artifact_relevance(artifact: &Artifact, agent_role: &str, agent_expertise: &[String]) -> f64 {
    let role_lower = agent_role.to_lowercase();
    let mut score: f64 = 0.0;

    // Type-based relevance
    match artifact.artifact_type {
        ArtifactType::Spec => {
            if role_lower.contains("pm")
                || role_lower.contains("product")
                || role_lower.contains("architect")
            {
                score += 0.6;
            } else {
                score += 0.3;
            }
        }
        ArtifactType::Architecture => {
            if role_lower.contains("architect")
                || role_lower.contains("tech_lead")
                || role_lower.contains("senior")
                || role_lower.contains("lead")
            {
                score += 0.6;
            } else {
                score += 0.2;
            }
        }
        ArtifactType::Ticket => {
            if role_lower.contains("dev")
                || role_lower.contains("qa")
                || role_lower.contains("engineer")
            {
                score += 0.6;
            } else {
                score += 0.2;
            }
        }
        ArtifactType::Notes => {
            score += 0.2;
        }
        ArtifactType::Decision => {
            score += 0.3;
        }
        ArtifactType::Snippet => {
            score += 0.2;
        }
    }

    // Keyword match: +0.2 per expertise tag found in title or content
    let title_lower = artifact.title.to_lowercase();
    let content_lower = artifact.content.to_lowercase();
    for tag in agent_expertise {
        let tag_lower = tag.to_lowercase();
        if title_lower.contains(&tag_lower) || content_lower.contains(&tag_lower) {
            score += 0.2;
        }
    }

    // Clamp to 1.0
    score.min(1.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(name: &str) -> AgentId {
        AgentId::new(name)
    }

    #[test]
    fn record_and_retrieve() {
        let mut mgr =
            TeamContextManager::new("team-1", "session-1", None, ContextViewConfig::default());

        mgr.record_turn(
            make_agent("alice"),
            "architect",
            "We should use a microservices pattern.",
        );
        mgr.record_turn(
            make_agent("bob"),
            "dev",
            "I agree. Let me draft the API schema.",
        );
        mgr.record_turn(make_agent("carol"), "qa", "I'll prepare the test plan.");

        assert_eq!(mgr.turn_count(), 3);
        assert_eq!(mgr.history().len(), 3);
        assert_eq!(mgr.history()[0].speaker, make_agent("alice"));
        assert_eq!(mgr.history()[2].speaker, make_agent("carol"));
    }

    #[test]
    fn context_view_caching() {
        let mut mgr =
            TeamContextManager::new("team-1", "session-1", None, ContextViewConfig::default());

        mgr.record_turn(make_agent("alice"), "architect", "Initial message.");

        let agent = make_agent("alice");
        let view1 = mgr.context_for(&agent, "architect", &[], 10000);
        let gen1 = mgr.generation;

        // Same generation — should be cached (same estimated_tokens as proxy for identity)
        let view2 = mgr.context_for(&agent, "architect", &[], 10000);
        assert_eq!(view1.estimated_tokens, view2.estimated_tokens);
        assert_eq!(mgr.generation, gen1);

        // New turn invalidates cache
        mgr.record_turn(make_agent("bob"), "dev", "Another message.");
        let view3 = mgr.context_for(&agent, "architect", &[], 10000);
        assert_ne!(mgr.generation, gen1);
        // Content changed, so recent_turns count differs
        assert_eq!(view3.recent_turns.len(), 2);
    }

    #[test]
    fn own_contributions_always_full() {
        let config = ContextViewConfig {
            recent_full_turns: 3,
            ..Default::default()
        };
        let mut mgr = TeamContextManager::new("team-1", "session-1", None, config);

        let alice = make_agent("alice");
        let bob = make_agent("bob");
        let carol = make_agent("carol");

        // 20 turns, cycling through 3 agents
        for i in 0..20 {
            let agent = match i % 3 {
                0 => &alice,
                1 => &bob,
                _ => &carol,
            };
            mgr.record_turn(
                agent.clone(),
                "dev",
                format!("Turn {i} content from {}", agent.as_str()),
            );
        }

        assert_eq!(mgr.turn_count(), 20);

        // Alice has 7 turns (0,3,6,9,12,15,18)
        let view = mgr.context_for(&alice, "dev", &[], 100000);
        assert_eq!(view.own_contributions.len(), 7);

        // Bob has 7 turns (1,4,7,10,13,16,19)
        let view = mgr.context_for(&bob, "dev", &[], 100000);
        assert_eq!(view.own_contributions.len(), 7);

        // Carol has 6 turns (2,5,8,11,14,17)
        let view = mgr.context_for(&carol, "dev", &[], 100000);
        assert_eq!(view.own_contributions.len(), 6);

        // Only 3 recent turns despite 20 total
        assert_eq!(view.recent_turns.len(), 3);
    }

    #[test]
    fn artifact_relevance_scoring() {
        use crate::workspace::{Artifact, ArtifactStatus, ArtifactType};

        let spec = Artifact {
            id: "art-1".into(),
            artifact_type: ArtifactType::Spec,
            title: "API Spec".into(),
            content: "REST API specification for the user service with rust endpoints.".into(),
            created_by: make_agent("pm"),
            last_modified_by: make_agent("pm"),
            status: ArtifactStatus::Draft,
            version: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        // PM gets high relevance for Spec
        let score = artifact_relevance(&spec, "pm", &[]);
        assert!(
            score >= 0.5,
            "PM should have high spec relevance, got {score}"
        );

        // Architect also high
        let score = artifact_relevance(&spec, "architect", &[]);
        assert!(
            score >= 0.5,
            "Architect should have high spec relevance, got {score}"
        );

        // Dev gets lower base relevance
        let score_base = artifact_relevance(&spec, "dev", &[]);
        assert!(
            score_base < 0.5,
            "Dev should have lower spec relevance, got {score_base}"
        );

        // But expertise tags boost it
        let score_with_expertise = artifact_relevance(&spec, "dev", &["rust".into(), "api".into()]);
        assert!(
            score_with_expertise > score_base,
            "Expertise should boost relevance: {score_with_expertise} vs {score_base}"
        );

        // Architecture artifact + architect role
        let arch = Artifact {
            artifact_type: ArtifactType::Architecture,
            title: "System Architecture".into(),
            content: "Microservices architecture with database sharding.".into(),
            ..spec.clone()
        };
        let score = artifact_relevance(&arch, "tech_lead", &[]);
        assert!(
            score >= 0.5,
            "Tech lead should have high arch relevance, got {score}"
        );

        // Ticket artifact + dev role
        let ticket = Artifact {
            artifact_type: ArtifactType::Ticket,
            title: "Implement auth endpoint".into(),
            content: "Add JWT authentication to the login endpoint.".into(),
            ..spec
        };
        let score = artifact_relevance(&ticket, "senior_dev", &[]);
        assert!(
            score >= 0.5,
            "Senior dev should have high ticket relevance, got {score}"
        );
    }

    #[test]
    fn context_view_respects_token_limit() {
        let config = ContextViewConfig {
            recent_full_turns: 5,
            older_summary_max_tokens: 500,
            ..Default::default()
        };
        let mut mgr = TeamContextManager::new("team-1", "session-1", None, config);

        let alice = make_agent("alice");

        // Add many turns with substantial content
        for i in 0..50 {
            let content = format!("Turn {i}: {}", "x".repeat(200));
            mgr.record_turn(alice.clone(), "dev", content);
        }

        // Request with a tight token limit
        let view = mgr.context_for(&alice, "dev", &[], 2000);

        // The older_summary should be truncated to fit
        let summary_tokens = estimate_tokens(&view.older_summary);
        assert!(
            summary_tokens <= 500,
            "Older summary should respect max_tokens: {summary_tokens}"
        );
    }

    #[test]
    fn first_sentence_extraction() {
        assert_eq!(
            first_sentence_of("Hello world. More text here."),
            "Hello world."
        );
        assert_eq!(first_sentence_of("No period"), "No period");
        assert_eq!(first_sentence_of("Question? Yes indeed."), "Question?");
    }

    #[test]
    fn empty_history_produces_empty_view() {
        let mut mgr =
            TeamContextManager::new("team-1", "session-1", None, ContextViewConfig::default());
        let agent = make_agent("alice");
        let view = mgr.context_for(&agent, "dev", &[], 10000);

        assert!(view.recent_turns.is_empty());
        assert!(view.older_summary.is_empty());
        assert!(view.own_contributions.is_empty());
        assert!(view.artifact_context.is_empty());
        assert_eq!(view.estimated_tokens, 0);
    }
}
