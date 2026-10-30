/// NATS subject hierarchy for the workforce system.
///
/// All subjects are prefixed with `workforce.` to namespace them.
///2 for the full hierarchy.
/// Subject for agent direct messages (inbox).
pub fn agent_inbox(agent_id: &str) -> String {
    format!("workforce.agents.{agent_id}.inbox")
}

/// Subject for agent status/heartbeat updates.
pub fn agent_status(agent_id: &str) -> String {
    format!("workforce.agents.{agent_id}.status")
}

/// Subject for interactive chat requests to an agent session.
pub fn agent_chat(agent_id: &str, session_id: &str) -> String {
    format!("workforce.agents.{agent_id}.chat.{session_id}")
}

/// Wildcard subject for all chat requests to an agent.
pub fn agent_chat_all(agent_id: &str) -> String {
    format!("workforce.agents.{agent_id}.chat.>")
}

/// Subject for streaming progress from an agent session.
pub fn agent_progress(agent_id: &str, session_id: &str) -> String {
    format!("workforce.agents.{agent_id}.progress.{session_id}")
}

/// Subject for agent tool invocations.
pub fn agent_tool(agent_id: &str, tool_name: &str) -> String {
    format!("workforce.agents.{agent_id}.tools.{tool_name}")
}

/// Subject for team channel messages.
pub fn team_channel(team_id: &str, channel: &str) -> String {
    format!("workforce.teams.{team_id}.messages.{channel}")
}

/// Subject for team task updates.
pub fn team_task(team_id: &str, task_id: &str) -> String {
    format!("workforce.teams.{team_id}.tasks.{task_id}")
}

/// Subject for team events (join, leave, status changes).
pub fn team_events(team_id: &str) -> String {
    format!("workforce.teams.{team_id}.events")
}

/// Subject for policy evaluation requests (request/reply).
pub fn policy_evaluate() -> String {
    "workforce.system.policy.evaluate".to_string()
}

/// Subject for policy change notifications.
pub fn policy_updates() -> String {
    "workforce.system.policy.updates".to_string()
}

/// Subject for orchestrator commands.
pub fn orchestrator_commands() -> String {
    "workforce.system.orchestrator.commands".to_string()
}

/// Subject for node heartbeats.
pub fn node_heartbeat(node_id: &str) -> String {
    format!("workforce.system.nodes.{node_id}.heartbeat")
}

/// Subject for node capability announcements.
pub fn node_capabilities(node_id: &str) -> String {
    format!("workforce.system.nodes.{node_id}.capabilities")
}

/// Subject for plugin manifests.
pub fn plugin_manifest(plugin_name: &str) -> String {
    format!("workforce.system.plugins.{plugin_name}.manifest")
}

/// Subject for plugin tool invocations.
pub fn plugin_tool(plugin_name: &str, tool_name: &str) -> String {
    format!("workforce.tools.{plugin_name}.{tool_name}")
}

/// Subject for inbound events from event sources.
pub fn event_source(source_type: &str, event_kind: &str) -> String {
    format!("workforce.events.{source_type}.{event_kind}")
}

// ── Knowledge subjects ──────────────────────────────────────────────

/// Subject for triggering a knowledge index of a repository.
pub fn knowledge_index(repo_id: &str) -> String {
    format!("workforce.knowledge.{repo_id}.index")
}

/// Subject for knowledge indexing status/progress updates.
pub fn knowledge_status(repo_id: &str) -> String {
    format!("workforce.knowledge.{repo_id}.status")
}

/// Subject for knowledge query requests (request/reply pattern).
pub fn knowledge_query() -> String {
    "workforce.knowledge.query".to_string()
}

/// Wildcard for all knowledge events for a specific repo.
pub fn knowledge_all(repo_id: &str) -> String {
    format!("workforce.knowledge.{repo_id}.>")
}

/// Wildcard for all knowledge events across all repos.
pub fn all_knowledge_messages() -> String {
    "workforce.knowledge.>".to_string()
}

/// Subject for controlling a live turn (cancel, follow-up, approval answer).
/// Only the instance running the turn subscribes, so a request with nobody
/// holding the turn fails fast with no responders. Deliberately outside
/// `workforce.agents.>`: a JetStream-captured subject is acknowledged by the
/// stream itself, which would read as a reply from the turn.
pub fn turn_control(agent_id: &str, session_id: &str) -> String {
    format!("workforce.hub.control.{agent_id}.{session_id}")
}

// ── Node subjects ───────────────────────────────────────────────────

/// Subject for reaching a person's node (request/reply). Only the instance
/// holding the node's connection subscribes, so a request with nobody
/// holding it fails fast with no responders. Outside every stream, as
/// [`turn_control`] is.
pub fn node(key: &str) -> String {
    format!("workforce.hub.node.{key}")
}

/// Subject the holder of a node publishes one call's events to. The caller
/// mints the request id, a UUID, and subscribes before it sends the call.
pub fn node_reply(request_id: &str) -> String {
    format!("workforce.hub.node.reply.{request_id}")
}

// ── Voice subjects ──────────────────────────────────────────────────

/// Subject for voice announcements (request/reply pattern). Every hub
/// instance's gateway subscribes; an instance whose local registry has a
/// voice-bridge connection admitting the target forwards the announcement
/// and replies, so the requester learns whether anything will speak it.
pub fn voice_announce() -> String {
    "workforce.system.voice.announce".to_string()
}

// ── Job subjects ────────────────────────────────────────────────────

/// Subject for job completion notifications for an agent.
pub fn agent_job_completed(agent_id: &str) -> String {
    format!("workforce.jobs.{agent_id}.completed")
}

/// Subject for job progress updates for an agent.
pub fn agent_job_progress(agent_id: &str) -> String {
    format!("workforce.jobs.{agent_id}.progress")
}

/// Wildcard for all job notifications for an agent.
pub fn agent_jobs_all(agent_id: &str) -> String {
    format!("workforce.jobs.{agent_id}.>")
}

/// Wildcard for all agent messages.
pub fn all_agent_messages() -> String {
    "workforce.agents.>".to_string()
}

/// Wildcard for all team messages.
pub fn all_team_messages() -> String {
    "workforce.teams.>".to_string()
}

/// Wildcard for all system messages.
pub fn all_system_messages() -> String {
    "workforce.system.>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_outside_every_stream(subject: &str) {
        crate::streams::STREAMED_SUBJECTS
            .iter()
            .map(|wildcard| wildcard.trim_end_matches('>'))
            .for_each(|root| {
                assert!(
                    !subject.starts_with(root),
                    "{subject} is captured by {root}>"
                )
            });
    }

    #[test]
    fn turn_control_is_outside_every_stream() {
        let subject = turn_control("dev-1", "sess-1");
        assert_eq!(subject, "workforce.hub.control.dev-1.sess-1");
        assert_outside_every_stream(&subject);
    }

    #[test]
    fn node_subjects_are_outside_every_stream() {
        let request_id = "6f1c2e4a-0b7d-4c1e-9a55-3f2d8e9b1a00";
        assert_eq!(node("studio"), "workforce.hub.node.studio");
        assert_eq!(
            node_reply(request_id),
            format!("workforce.hub.node.reply.{request_id}")
        );
        for subject in [
            node("studio"),
            node("desk:0b6a5f3e-1c2d-4e5f-8a9b-0c1d2e3f4a5b"),
            node_reply(request_id),
        ] {
            assert_outside_every_stream(&subject);
        }
    }

    #[test]
    fn subject_formats() {
        assert_eq!(agent_inbox("dev-1"), "workforce.agents.dev-1.inbox");
        assert_eq!(agent_status("dev-1"), "workforce.agents.dev-1.status");
        assert_eq!(
            agent_tool("dev-1", "filesystem.read"),
            "workforce.agents.dev-1.tools.filesystem.read"
        );
        assert_eq!(
            team_channel("backend", "general"),
            "workforce.teams.backend.messages.general"
        );
        assert_eq!(policy_evaluate(), "workforce.system.policy.evaluate");
        assert_eq!(
            node_heartbeat("kings-macbook"),
            "workforce.system.nodes.kings-macbook.heartbeat"
        );
        assert_eq!(
            event_source("email", "incoming"),
            "workforce.events.email.incoming"
        );
    }

    #[test]
    fn knowledge_subject_formats() {
        assert_eq!(
            knowledge_index("driftwerk-abc12345"),
            "workforce.knowledge.driftwerk-abc12345.index"
        );
        assert_eq!(
            knowledge_status("driftwerk-abc12345"),
            "workforce.knowledge.driftwerk-abc12345.status"
        );
        assert_eq!(knowledge_query(), "workforce.knowledge.query");
        assert_eq!(
            knowledge_all("driftwerk-abc12345"),
            "workforce.knowledge.driftwerk-abc12345.>"
        );
        assert_eq!(all_knowledge_messages(), "workforce.knowledge.>");
    }
}
