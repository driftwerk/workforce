//! Whether an agent would load a workspace `config.toml`, decided before the
//! file is written, by the same resolution boot and hot-reload apply.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use workforce_agent::llm::provider::ProviderKind;
use workforce_common::settings::{
    AdmitWorkspace, AgentConfig, AgentWorkspaceConfig, LlmConfig, WorkforceConfig,
};

use crate::hub_core::subscriptions::{review_agent_llm, Finding, Install};
use crate::resolved_config::ResolvedConfig;
use crate::serve::AgentService;

/// A workspace config as the agent would run it, resolved without swapping
/// anything in.
pub struct Prospective {
    pub effective: LlmConfig,
    pub next: ResolvedConfig,
    pub provider_change: Option<(ProviderKind, String)>,
    pub equivalences: Option<HashMap<String, Vec<String>>>,
}

/// Why a workspace config was not admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRefusal {
    /// The body does not parse, or names a provider that does not exist.
    Parse(String),
    /// A setting holds what it can't, said in plain words for the person
    /// who set it (`[oversight]`).
    Said(String),
    /// The agent would not load it: every finding, the refused ones first.
    Refused(Vec<Finding>),
}

impl fmt::Display for ConfigRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(reason) | Self::Said(reason) => f.write_str(reason),
            Self::Refused(findings) => findings
                .iter()
                .find(|finding| finding.is_refused())
                .map_or(Ok(()), |refused| f.write_str(&refused.message)),
        }
    }
}

impl std::error::Error for ConfigRefusal {}

/// What decides whether one agent would load a workspace config: the install
/// config its agent section and defaults come from, and its live tier.
#[derive(Clone)]
pub struct WorkspaceAdmission {
    config: Arc<WorkforceConfig>,
    agent_id: String,
    tier_key: Arc<parking_lot::RwLock<String>>,
}

impl WorkspaceAdmission {
    pub fn new(
        config: Arc<WorkforceConfig>,
        agent_id: &str,
        tier_key: Arc<parking_lot::RwLock<String>>,
    ) -> Self {
        Self {
            config,
            agent_id: agent_id.to_string(),
            tier_key,
        }
    }

    /// Admission for an agent that is not running yet (one being imported),
    /// at the tier its section boots it on.
    pub fn for_new_agent(config: &WorkforceConfig, agent: &AgentConfig) -> eyre::Result<Self> {
        let (_, tier_key) = crate::hub_core::subscriptions::agent_provider(
            &crate::serve::merged_effective_llm(Some(agent), None, &config.llm).provider,
        )?;
        let mut config = config.clone();
        config.agents.retain(|existing| existing.id != agent.id);
        config.agents.push(agent.clone());
        Ok(Self::new(
            Arc::new(config),
            agent.id.as_str(),
            Arc::new(parking_lot::RwLock::new(tier_key)),
        ))
    }

    pub fn agent_config(&self) -> Option<&AgentConfig> {
        self.config
            .agents
            .iter()
            .find(|agent| agent.id.as_str() == self.agent_id)
    }

    /// `workspace` resolved against the agent's section and the install, with
    /// the provider and tier it would switch to.
    pub fn prospective(
        &self,
        workspace: Option<&AgentWorkspaceConfig>,
    ) -> eyre::Result<Prospective> {
        let llm = workspace.and_then(|ws| ws.llm.as_ref());
        let provider_change = llm
            .and_then(|llm| llm.provider.as_deref())
            .map(workforce_agent::llm::tiers::parse_provider_spec)
            .transpose()?
            .flatten();
        let tier_key = provider_change
            .as_ref()
            .map(|(_, tier)| tier.clone())
            .unwrap_or_else(|| self.tier_key.read().clone());
        let equivalences = llm.and_then(|llm| llm.equivalences.clone());
        let agent = self.agent_config();
        Ok(Prospective {
            effective: crate::serve::merged_effective_llm(agent, workspace, &self.config.llm),
            next: ResolvedConfig::resolve(
                agent,
                workspace,
                &self.config,
                &tier_key,
                equivalences.as_ref(),
            ),
            provider_change,
            equivalences,
        })
    }

    /// The findings on `workspace`, or why the agent would not load it.
    fn review(&self, workspace: &AgentWorkspaceConfig) -> Result<Vec<Finding>, ConfigRefusal> {
        workspace.oversight().map_err(ConfigRefusal::Said)?;
        let prospective = self
            .prospective(Some(workspace))
            .map_err(|e| ConfigRefusal::Parse(e.to_string()))?;
        let findings = review_agent_llm(
            &prospective.effective,
            &prospective.next,
            &Install::of(&self.config),
        );
        if findings.iter().any(Finding::is_refused) {
            Err(ConfigRefusal::Refused(findings))
        } else {
            Ok(findings)
        }
    }

    /// [`Self::review`] of a `config.toml` body, parsed as boot reads it.
    pub fn review_toml(&self, body: &str) -> Result<Vec<Finding>, ConfigRefusal> {
        let workspace = AgentWorkspaceConfig::parse(body)
            .map_err(|e| ConfigRefusal::Parse(format!("invalid config.toml: {e}")))?;
        self.review(&workspace)
    }
}

impl AdmitWorkspace for WorkspaceAdmission {
    fn admit(&self, workspace: &AgentWorkspaceConfig) -> eyre::Result<()> {
        self.review(workspace).map(drop).map_err(eyre::Report::new)
    }
}

/// The findings on a `config.toml` body, or why this agent would not
/// load it: the check every config write passes before it lands.
pub fn admit_workspace(service: &AgentService, body: &str) -> Result<Vec<Finding>, ConfigRefusal> {
    service.admission.review_toml(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_core::subscriptions::Severity;

    #[test]
    fn oversight_is_admitted_in_either_form_and_refused_in_plain_words() {
        let admission = WorkspaceAdmission::new(
            Arc::new(WorkforceConfig::default()),
            "desk",
            Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
        );
        admission
            .review_toml(
                "[oversight]\nenabled = true\nlevel = \"watch\"\nwake = { quiet_minutes = 0 }\n",
            )
            .unwrap();
        admission
            .review_toml("[oversight]\nenabled = true\n[oversight.wake]\nwaiting_minutes = 0\n")
            .unwrap();
        let refused = admission
            .review_toml("[oversight]\nlevel = \"boss\"\n")
            .unwrap_err();
        assert_eq!(
            refused,
            ConfigRefusal::Said(
                "How much the overseer does on its own must be watch, suggest or act.".into()
            )
        );
        assert!(matches!(
            admission.review_toml("[oversight\n"),
            Err(ConfigRefusal::Parse(_))
        ));
    }

    #[test]
    fn source_agent_on_an_agent_that_is_no_copy_is_a_warning() {
        let admission = WorkspaceAdmission::new(
            Arc::new(WorkforceConfig::default()),
            "analyst",
            Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
        );
        let source = "[know_how]\nsource_agent = \"other\"\n";
        let findings = admission.review_toml(source).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].message.contains("is ignored"));
        let copy = format!("[evals]\ncopy = true\n{source}");
        assert!(admission.review_toml(&copy).unwrap().is_empty());
    }
}
