//! This agent's know-how: the sites it reads, who may reach them, and the
//! preview it shows.

use std::sync::Arc;

use crate::serve::AgentService;

/// The Metabase sites `principal` can reach, read under their identity,
/// with the instance the session's active environment pins Metabase to.
pub(crate) async fn sites(
    service: &AgentService,
    session: Option<&workforce_common::session::Session>,
    principal: &str,
) -> crate::know_how::Sites {
    let Some(vault) = service.vault.clone() else {
        return crate::know_how::Sites::default();
    };
    let global = service.global_vault.clone();
    let (instances, default) = workforce_agent::runner::CURRENT_USER_ID
        .sync_scope(principal.to_string(), move || {
            crate::skill_loader::metabase_sites_for(vault, global)
        });
    let pinned = crate::environments::resolve_scope(service, session)
        .skill_instances
        .get("metabase")
        .and_then(|set| set.first().cloned());
    crate::know_how::Sites {
        instances,
        default,
        pinned,
    }
}

/// What `person` can open anyway on this agent, for know-how: the sites
/// their access reaches, the repos their code tools read and the
/// agent's policy.
pub(crate) async fn access(
    service: &AgentService,
    session: Option<&workforce_common::session::Session>,
    person: &str,
) -> crate::know_how::Access {
    crate::know_how::Access {
        sites: sites(service, session, person).await,
        repos: workforce_agent::runner::CURRENT_USER_ID.sync_scope(person.to_string(), || {
            crate::know_how::code::readable_repos(
                crate::know_how::code::code_index(),
                &service.tools,
                service.permission_evaluator.read().clone(),
                service.agent_id.as_str(),
            )
        }),
        policy: Some(Arc::clone(&service.policy)),
        admits: Some(crate::hosts::admits_through(Arc::clone(
            &service.agent_registry,
        ))),
    }
}

/// What `person`'s eval turns on this agent would be shown of its
/// know-how, under `own` in place of its `config.toml` when given (see
/// [`crate::know_how::preview`]).
pub(crate) async fn preview(
    service: &AgentService,
    person: &str,
    own: Option<workforce_common::settings::AgentWorkspaceConfig>,
    questions: &[String],
) -> eyre::Result<crate::know_how::preview::Preview> {
    let own = own.or_else(|| service.resolved.snapshot().workspace.clone());
    let install = &service.config.know_how;
    let mut enabled = own.clone().unwrap_or_default();
    enabled
        .know_how
        .get_or_insert_with(Default::default)
        .enabled = Some(true);
    let kh = crate::know_how::KnowHow::shared(&service.config).await;
    let access = access(service, None, person).await;
    kh.preview(
        service.agent_id.as_str(),
        install.for_workspace(own.as_ref()).as_ref(),
        install.for_workspace(Some(&enabled)).as_ref(),
        person,
        &access,
        questions,
    )
    .await
}
