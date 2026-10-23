//! The session context a remote MCP caller gets from the service.

use eyre::Result;

use crate::serve::AgentService;

/// The task-local scope a remote MCP call runs in: the caller's persistent
/// `mcp:<identity>` session on this agent, so tool calls and `agent.chat`
/// share one thread of history and one approval/secret-marker key.
pub(crate) async fn session_context(
    service: &AgentService,
    identity_id: &str,
) -> Result<workforce_agent::runner::SessionContext> {
    let channel = crate::remote_mcp::server::session_channel(identity_id);
    let session =
        service
            .sessions
            .get_or_create(&channel, identity_id, service.agent_id.as_str())?;
    let env = crate::environments::resolve_scope(service, Some(&session));
    Ok(workforce_agent::runner::SessionContext {
        key: Some(channel),
        id: Some(session.id),
        user_id: Some(identity_id.to_string()),
        platform_user_id: Some(service.resolve_platform_user_id(identity_id)),
        agent_id: Some(service.agent_id.to_string()),
        active_mcp_envs: env.mcp_envs,
        active_environment: env.environment,
        active_skill_instances: env.skill_instances,
        ..Default::default()
    })
}
