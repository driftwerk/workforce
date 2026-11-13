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
        workspace.computer().map_err(ConfigRefusal::Said)?;
        self.works_in(workspace)?;
        let connectors = self.connectors(workspace)?;
        let prospective = self
            .prospective(Some(workspace))
            .map_err(|e| ConfigRefusal::Parse(e.to_string()))?;
        let mut findings = review_agent_llm(
            &prospective.effective,
            &prospective.next,
            &Install::of(&self.config),
        );
        findings.extend(connectors);
        if findings.iter().any(Finding::is_refused) {
            Err(ConfigRefusal::Refused(findings))
        } else {
            Ok(findings)
        }
    }

    /// The folders the agent works in, each where an agent may: never `/`,
    /// a volume, home or above it, nor what is kept from every agent.
    fn works_in(&self, workspace: &AgentWorkspaceConfig) -> Result<(), ConfigRefusal> {
        let folders = workspace.works_in().map_err(ConfigRefusal::Said)?;
        if folders.is_empty() {
            return Ok(());
        }
        let home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let withheld = workforce_common::computer_access::withheld_on(
            std::path::Path::new(&self.config.hub.data_dir),
            &home,
        );
        match folders.iter().find_map(|folder| {
            workforce_common::computer_access::works_in_refusal(folder, &home, &withheld)
        }) {
            Some(refusal) => Err(ConfigRefusal::Said(refusal)),
            None => Ok(()),
        }
    }

    /// What the agent's `[skills]` says. Only a `[desk]` hub reads it, so
    /// only there is a malformed one refused; elsewhere it is ignored as it
    /// always was, and an empty `enabled`, which would turn every connector
    /// off on a `[desk]` hub, is said so nobody is surprised by it later.
    fn connectors(&self, workspace: &AgentWorkspaceConfig) -> Result<Vec<Finding>, ConfigRefusal> {
        let read = workspace.connectors();
        if self.config.desk.enabled {
            return read.map(|_| Vec::new()).map_err(ConfigRefusal::Said);
        }
        Ok(match read {
            Ok(binding) if binding.enabled.as_ref().is_some_and(Vec::is_empty) => {
                vec![Finding::warning(
                    "skills",
                    "[skills] enabled = [] turns every connector off, but only on a hub \
                     running Desk; this hub ignores it."
                        .into(),
                )]
            }
            _ => Vec::new(),
        })
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
                "How much the overseer does on its own must be watch, suggest, act or full.".into()
            )
        );
        assert!(matches!(
            admission.review_toml("[oversight\n"),
            Err(ConfigRefusal::Parse(_))
        ));
    }

    /// `[computer]` is admitted on any hub, `[desk]` or not, and a config
    /// without it admits as before; what is wrong is said in plain words.
    #[test]
    fn computer_is_admitted_and_refused_in_plain_words() {
        let admission = WorkspaceAdmission::new(
            Arc::new(WorkforceConfig::default()),
            "allan",
            Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
        );
        assert!(admission.review_toml("").unwrap().is_empty());
        for ok in [
            "[computer]\naccess = \"own_folder\"\n",
            "[computer]\naccess = \"full\"\nautonomous = true\nask_in_own_folder = true\n",
        ] {
            assert!(admission.review_toml(ok).unwrap().is_empty(), "{ok}");
        }
        assert_eq!(
            admission
                .review_toml("[computer]\naccess = \"own_folder\"\nautonomous = true\n")
                .unwrap_err(),
            ConfigRefusal::Said(
                "An agent can work on its own only with full access to this computer.".into()
            )
        );
        let unchanged: AgentWorkspaceConfig = AgentWorkspaceConfig::parse("").unwrap();
        assert!(unchanged.computer.is_none());
        assert!(!toml::to_string(&unchanged).unwrap().contains("computer"));
    }

    #[test]
    fn works_in_is_admitted_inside_home_and_refused_above_it() {
        let data = tempfile::tempdir().unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = data.path().join("Desk/hub/data").display().to_string();
        let admission = WorkspaceAdmission::new(
            Arc::new(config),
            "allan",
            Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
        );
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let inside = format!("{home}/dev/acme-web");
        assert!(admission
            .review_toml(&format!("[computer]\nworks_in = [\"{inside}\"]\n"))
            .unwrap()
            .is_empty());
        for wide in ["/".to_string(), home.clone()] {
            let e = admission
                .review_toml(&format!("[computer]\nworks_in = [\"{wide}\"]\n"))
                .unwrap_err();
            assert!(
                e.to_string().contains("isn't inside your home"),
                "{wide} → {e}"
            );
        }
        let desk = data.path().join("Desk/hub/data/agents/ben");
        let e = admission
            .review_toml(&format!(
                "[computer]\nworks_in = [\"{}\"]\n",
                desk.display()
            ))
            .unwrap_err();
        // Outside home here; inside it, what Desk keeps is refused too
        // (`computer_access`'s own tests).
        assert!(
            e.to_string().contains("Desk's own data") || e.to_string().contains("inside your home"),
            "{e}"
        );
        let e = admission
            .review_toml(&format!("[computer]\nworks_in = [\"{home}/.ssh\"]\n"))
            .unwrap_err();
        assert!(e.to_string().contains("keys, sign-ins"), "{e}");
        let e = admission
            .review_toml("[computer]\nworks_in = [\"dev/acme\"]\n")
            .unwrap_err();
        assert!(e.to_string().contains("full folder path"), "{e}");
    }

    /// `[skills]` is checked only where it is read: a `[desk]` hub refuses a
    /// malformed one in plain words; any other hub admits what it admitted
    /// before, and only says that `enabled = []` does nothing there.
    #[test]
    fn skills_are_checked_on_a_desk_hub_and_left_alone_elsewhere() {
        let admission = |desk: bool| {
            let mut config = WorkforceConfig::default();
            config.desk.enabled = desk;
            WorkspaceAdmission::new(
                Arc::new(config),
                "allan",
                Arc::new(parking_lot::RwLock::new("anthropic".to_string())),
            )
        };
        let (desk, server) = (admission(true), admission(false));
        for ok in [
            "",
            "[skills]\nenabled = [\"linear\"]\nread_only = [\"linear\"]\nown_credential = [\"linear\"]\n",
            "[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\"]\n",
        ] {
            assert!(desk.review_toml(ok).unwrap().is_empty(), "{ok}");
            assert!(server.review_toml(ok).unwrap().is_empty(), "{ok}");
        }
        let malformed = "[skills]\nenabled = \"linear\"\n";
        assert!(matches!(
            desk.review_toml(malformed),
            Err(ConfigRefusal::Said(_))
        ));
        assert!(server.review_toml(malformed).unwrap().is_empty());

        let none = "[skills]\nenabled = []\n";
        assert!(desk.review_toml(none).unwrap().is_empty());
        let said = server.review_toml(none).unwrap();
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].severity, Severity::Warning);
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
