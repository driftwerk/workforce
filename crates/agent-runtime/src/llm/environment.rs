//! Environments: pin an agent's multi-instance skills (and MCP workspaces) to
//! a coherent target so the agent can't muddle prod and staging data in a single
//! answer. The MCP half reuses [`super::mcp_workspace`]; this module is the
//! skill half, the **instance lock**.
//!
//! While an environment is active ([`crate::runner::ACTIVE_ENVIRONMENT`] is
//! set), a skill it binds is held to its bound set of instances: a call naming
//! none gets the set's first, a call naming one outside it is refused. A skill it
//! does not bind, and every skill when no environment is active, keeps per-call
//! select-or-default, so every adapter resolver routes through
//! [`resolve_instance`] unconditionally.
//!
//! Which instances an agent's skills expose is per agent: each skill sets an
//! [`InstanceSource`] on the agent's own tool registry.

use std::collections::HashMap;

/// An environment's skill bindings: each bound skill's instances, the first
/// being the one a call naming none gets.
pub type SkillPins = HashMap<String, Vec<String>>;

/// A refused call: the environment lock declined the requested instance. The
/// adapter surfaces [`Self::message`] to the agent as a tool failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DormantInstance {
    pub message: String,
}

/// Resolve which named instance of a multi-instance skill a call should hit.
///
/// Unbound, or with no environment active, returns `requested ?? default`
/// unvalidated, so the caller's own `clients.get(name)` lookup produces its usual
/// "unknown instance" error. Bound, a `requested` outside the set is refused and
/// the chosen pin must be one the skill loaded.
pub fn resolve_instance<V>(
    skill: &str,
    requested: Option<&str>,
    default: &str,
    clients: &HashMap<String, V>,
) -> Result<String, DormantInstance> {
    let free = || Ok(requested.unwrap_or(default).to_string());
    let Some(environment) = crate::runner::current_active_environment() else {
        return free();
    };
    let pins = crate::runner::current_active_skill_instances();
    let Some(bound) = pins.get(skill).filter(|set| !set.is_empty()) else {
        return free();
    };
    let pin = match requested {
        Some(req) if bound.iter().any(|b| b == req) => req,
        Some(req) => {
            return Err(DormantInstance {
                message: sideways_message(skill, &environment, bound, req),
            })
        }
        None => bound[0].as_str(),
    };
    if !clients.contains_key(pin) {
        return Err(DormantInstance {
            message: misconfigured_message(skill, &environment, pin, clients),
        });
    }
    Ok(pin.to_string())
}

fn sideways_message(skill: &str, environment: &str, bound: &[String], requested: &str) -> String {
    format!(
        "Environment `{environment}` binds `{skill}` to {}, so `{skill}` instance `{requested}` \
         is dormant and not reachable. You cannot change environments yourself — ask a human to \
         run `cmd: env use <name>`, or to add `{requested}` to this environment's `{skill}` \
         binding, then retry.",
        quoted_list(bound)
    )
}

fn misconfigured_message<V>(
    skill: &str,
    environment: &str,
    pin: &str,
    clients: &HashMap<String, V>,
) -> String {
    let mut available: Vec<&str> = clients.keys().map(String::as_str).collect();
    available.sort_unstable();
    format!(
        "Environment `{environment}` points `{skill}` at instance `{pin}`, which is not configured \
         for this skill (available: {available:?}). Ask a human to fix the environment binding or \
         load the `{pin}` credentials."
    )
}

fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("`{i}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A loaded multi-instance skill's selectable instances and its own default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillVariants {
    pub instances: Vec<String>,
    pub default: String,
}

impl SkillVariants {
    /// The variants of a skill exposing `instances`, sorted and deduplicated;
    /// `None` below two, since a single-instance skill has nothing to pin.
    pub fn of(instances: impl IntoIterator<Item = String>, default: &str) -> Option<Self> {
        let mut instances: Vec<String> = instances.into_iter().collect();
        instances.sort();
        instances.dedup();
        (instances.len() >= 2).then(|| Self {
            instances,
            default: default.to_string(),
        })
    }

    pub fn has(&self, instance: &str) -> bool {
        self.instances.iter().any(|i| i == instance)
    }
}

/// Where a skill's current instances come from: fixed when the skill was
/// loaded, or read live for a skill whose instances change without a reload.
pub trait InstanceSource: Send + Sync {
    fn variants(&self) -> Option<SkillVariants>;
}

impl InstanceSource for SkillVariants {
    fn variants(&self) -> Option<SkillVariants> {
        Some(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES};

    fn clients(names: &[&str]) -> HashMap<String, ()> {
        names.iter().map(|n| (n.to_string(), ())).collect()
    }

    fn with_env<T>(name: &str, pins: &[(&str, &[&str])], f: impl FnOnce() -> T) -> T {
        let pins: SkillPins = pins
            .iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|i| i.to_string()).collect()))
            .collect();
        ACTIVE_ENVIRONMENT.sync_scope(name.to_string(), || {
            ACTIVE_SKILL_INSTANCES.sync_scope(pins, f)
        })
    }

    #[test]
    fn no_environment_returns_requested_then_default() {
        let c = clients(&["prod", "staging"]);
        assert_eq!(
            resolve_instance("metabase", Some("staging"), "prod", &c).unwrap(),
            "staging"
        );
        assert_eq!(
            resolve_instance("metabase", None, "prod", &c).unwrap(),
            "prod"
        );
    }

    #[test]
    fn no_environment_passes_unknown_through_for_caller_to_reject() {
        let c = clients(&["prod"]);
        assert_eq!(
            resolve_instance("metabase", Some("nope"), "prod", &c).unwrap(),
            "nope"
        );
    }

    #[test]
    fn bound_set_defaults_to_its_first_and_honours_its_members() {
        let c = clients(&["driftwerk", "fabrikam-crm", "other"]);
        with_env(
            "work",
            &[("linear", &["driftwerk", "fabrikam-crm"])],
            || {
                assert_eq!(
                    resolve_instance("linear", None, "other", &c).unwrap(),
                    "driftwerk"
                );
                assert_eq!(
                    resolve_instance("linear", Some("fabrikam-crm"), "other", &c).unwrap(),
                    "fabrikam-crm"
                );
            },
        );
    }

    #[test]
    fn bound_set_refuses_an_instance_outside_it() {
        let c = clients(&["driftwerk", "fabrikam-crm", "other"]);
        let err = with_env(
            "work",
            &[("linear", &["driftwerk", "fabrikam-crm"])],
            || resolve_instance("linear", Some("other"), "driftwerk", &c),
        )
        .unwrap_err();
        assert!(err.message.contains("dormant"), "{}", err.message);
        assert!(
            err.message.contains("`driftwerk`, `fabrikam-crm`"),
            "{}",
            err.message
        );
    }

    #[test]
    fn unbound_skill_keeps_per_call_selection() {
        let c = clients(&["prod", "staging"]);
        with_env("harbor-staging", &[("jira", &["staging"])], || {
            assert_eq!(
                resolve_instance("metabase", None, "prod", &c).unwrap(),
                "prod"
            );
            assert_eq!(
                resolve_instance("metabase", Some("staging"), "prod", &c).unwrap(),
                "staging"
            );
        });
    }

    #[test]
    fn pin_absent_from_clients_is_a_configuration_error_naming_it() {
        let c = clients(&["fabrikam-crm"]);
        let err = with_env(
            "work",
            &[("linear", &["driftwerk", "fabrikam-crm"])],
            || resolve_instance("linear", None, "fabrikam-crm", &c),
        )
        .unwrap_err();
        assert!(err.message.contains("not configured"), "{}", err.message);
        assert!(err.message.contains("`driftwerk`"), "{}", err.message);
    }

    #[test]
    fn variants_of_drops_single_instance_and_sorts_and_dedups() {
        assert_eq!(SkillVariants::of(["solo".to_string()], "solo"), None);
        let v = SkillVariants::of(
            [
                "staging".to_string(),
                "prod".to_string(),
                "prod".to_string(),
            ],
            "prod",
        )
        .unwrap();
        assert_eq!(v.instances, vec!["prod", "staging"]);
        assert_eq!(v.default, "prod");
        assert!(v.has("staging"));
        assert!(!v.has("demo"));
    }
}
