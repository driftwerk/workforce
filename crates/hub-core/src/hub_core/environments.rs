//! The environments a configuration resolves to.

use std::collections::BTreeMap;
use std::collections::HashMap;
use workforce_agent::llm::environment::SkillPins;
use workforce_common::settings::AgentEnvironmentsConfig;
use workforce_common::settings::EnvironmentDef;
use workforce_common::settings::EnvironmentsConfig;

/// An agent's environments as its current config resolves them: empty and
/// disabled unless the install allows them and the agent turns them on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedEnvironments {
    /// The install's kill switch leaves environments to the agent.
    pub allowed: bool,
    pub enabled: bool,
    pub default: Option<String>,
    pub defs: BTreeMap<String, EnvironmentDef>,
}

/// The environment a turn runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Active<'a> {
    pub name: &'a str,
    pub def: &'a EnvironmentDef,
    /// Chosen by the session rather than being the agent's default.
    pub is_override: bool,
}

/// The per-turn environment scope: the active environment, if any, and the
/// expanded task-local maps to scope around the run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentScope {
    pub environment: Option<String>,
    pub mcp_envs: HashMap<String, String>,
    pub skill_instances: SkillPins,
}

impl ResolvedEnvironments {
    pub fn resolve(install: &EnvironmentsConfig, agent: Option<&AgentEnvironmentsConfig>) -> Self {
        let allowed = install.allowed();
        agent
            .filter(|agent| allowed && agent.enabled == Some(true))
            .map(|agent| Self {
                allowed,
                enabled: true,
                default: agent.default.clone(),
                defs: agent.defs.clone(),
            })
            .unwrap_or(Self {
                allowed,
                ..Self::default()
            })
    }

    /// The session's named environment when this agent has one by that name
    /// that is not a draft, else the agent's default. A stale name, or one the
    /// agent has since made a draft under, falls back to the default rather
    /// than unlocking.
    pub fn active(&self, override_name: Option<&str>) -> Option<Active<'_>> {
        let named = override_name
            .and_then(|name| self.defs.get_key_value(name))
            .filter(|(_, def)| !def.draft)
            .map(|(name, def)| Active {
                name,
                def,
                is_override: self.default.as_deref() != Some(name.as_str()),
            });
        named.or_else(|| {
            self.default
                .as_deref()
                .and_then(|name| self.defs.get_key_value(name))
                .map(|(name, def)| Active {
                    name,
                    def,
                    is_override: false,
                })
        })
    }

    /// Expand the active environment for a turn. The session's manual
    /// `cmd: use` MCP overrides win over the environment's MCP bindings.
    pub fn scope(
        &self,
        override_name: Option<&str>,
        manual_mcp_envs: HashMap<String, String>,
    ) -> EnvironmentScope {
        match self.active(override_name) {
            None => EnvironmentScope {
                environment: None,
                mcp_envs: manual_mcp_envs,
                skill_instances: SkillPins::new(),
            },
            Some(active) => EnvironmentScope {
                environment: Some(active.name.to_string()),
                mcp_envs: active
                    .def
                    .mcp
                    .clone()
                    .into_iter()
                    .chain(manual_mcp_envs)
                    .collect(),
                skill_instances: active.def.skills.clone().into_iter().collect(),
            },
        }
    }
}
