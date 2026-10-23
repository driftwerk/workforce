//! Arms: the named configurations a set is run against. An arm is an agent,
//! and optionally settings laid over that agent's `config.toml` for the run.
//! Overrides are only ever laid over a test copy: an agent whose own
//! `config.toml` says it is one (`[evals] copy = true`), and never one the
//! run also answers as a real agent, nor the install's default.

use std::collections::BTreeSet;
use std::path::Path;

use eyre::{bail, ensure, Result, WrapErr};
use serde::{Deserialize, Serialize};
use toml::{Table, Value};
use workforce_common::settings::AgentWorkspaceConfig;
use workforce_orchestrator::workflow_store::content_hash;

use super::set::is_name;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arm {
    pub name: String,
    pub agent: String,
    /// The agent this arm's agent is a test copy of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_of: Option<String>,
    /// Settings laid over the agent's `config.toml` while the arm runs.
    #[serde(default, skip_serializing_if = "Table::is_empty")]
    pub overrides: Table,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmsFile {
    arm: Vec<Arm>,
}

impl Arm {
    /// The one arm of a plain run: `agent` as it stands.
    pub fn of_agent(agent: &str) -> Self {
        Self {
            name: agent.to_string(),
            agent: agent.to_string(),
            copy_of: None,
            overrides: Table::new(),
        }
    }

    /// The arm's version: results are only ever read back for the agent and
    /// overrides that produced them.
    pub fn hash(&self) -> String {
        let overrides = serde_json::to_string(&self.overrides).expect("overrides serialise");
        content_hash(&format!("{}\n{overrides}", self.agent))
    }

    /// The agent's `config.toml` with the arm's overrides laid over it.
    pub fn laid_over(&self, config: &str) -> Result<String> {
        let mut merged: Table = config
            .parse()
            .wrap_err_with(|| format!("{}'s config.toml does not parse", self.agent))?;
        lay_over(&mut merged, &self.overrides);
        Ok(toml::to_string(&merged)?)
    }
}

fn lay_over(base: &mut Table, overrides: &Table) {
    for (key, value) in overrides {
        match (base.get_mut(key), value) {
            (Some(Value::Table(under)), Value::Table(over)) => lay_over(under, over),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Every setting `table` names, as a dotted path, empty tables and arrays
/// aside.
fn leaves(prefix: &str, table: &Table) -> Vec<String> {
    table
        .iter()
        .flat_map(|(key, value)| {
            let path = match prefix.is_empty() {
                true => key.clone(),
                false => format!("{prefix}.{key}"),
            };
            match value {
                Value::Table(inner) => leaves(&path, inner),
                Value::Array(items) if items.is_empty() => Vec::new(),
                _ => vec![path],
            }
        })
        .collect()
}

/// The overrides an agent's config would silently drop: a `config.toml`
/// ignores a key it does not know, and an arm built on one compares nothing.
fn unknown_settings(overrides: &Table) -> Result<Vec<String>> {
    let parsed = AgentWorkspaceConfig::parse(&toml::to_string(overrides)?)
        .wrap_err("the overrides are not a valid config.toml")?;
    let Value::Table(kept) = Value::try_from(parsed)? else {
        bail!("an agent config serialises as a table");
    };
    let kept: BTreeSet<String> = leaves("", &kept).into_iter().collect();
    Ok(leaves("", overrides)
        .into_iter()
        .filter(|path| !kept.contains(path))
        .collect())
}

/// The `config.toml` section that marks an agent as a test copy.
const MARKER_SECTION: &str = "evals";

/// Whether an agent with this `config.toml` says of itself that it is a
/// test copy (`[evals] copy = true`): the one thing that lets an arm lay
/// overrides over it.
pub fn is_test_copy(config: &str) -> bool {
    AgentWorkspaceConfig::parse(config)
        .ok()
        .and_then(|config| config.evals)
        .is_some_and(|evals| evals.copy)
}

fn check(arms: &[Arm]) -> Result<()> {
    ensure!(!arms.is_empty(), "at least one arm is needed");
    let mut names = BTreeSet::new();
    for arm in arms {
        ensure!(
            is_name(&arm.name) && names.insert(arm.name.as_str()),
            "arm name {:?} must be unique, in letters, digits, '-' or '_'",
            arm.name
        );
        ensure!(
            is_name(&arm.agent),
            "arm {:?}: agent {:?} must be an agent id",
            arm.name,
            arm.agent
        );
        ensure!(
            arm.copy_of.as_deref() != Some(arm.agent.as_str()),
            "arm {:?}: an agent is not a copy of itself",
            arm.name
        );
        if arm.overrides.is_empty() {
            continue;
        }
        ensure!(
            !arm.overrides.contains_key(MARKER_SECTION),
            "arm {:?}: [{MARKER_SECTION}] is the copy's own to say, not an arm's to override",
            arm.name
        );
        ensure!(
            arm.copy_of.is_some(),
            "arm {:?} has overrides, so its agent must be a test copy: name the original in \
             `copy_of`",
            arm.name
        );
        let real = arms
            .iter()
            .any(|other| other.copy_of.as_deref() == Some(&arm.agent))
            || arms
                .iter()
                .any(|other| other.agent == arm.agent && other.copy_of.is_none());
        ensure!(
            !real,
            "arm {:?} would change the settings of {:?}, which this run also uses as a real agent",
            arm.name,
            arm.agent
        );
        let unknown =
            unknown_settings(&arm.overrides).wrap_err_with(|| format!("arm {:?}", arm.name))?;
        ensure!(
            unknown.is_empty(),
            "arm {:?} overrides settings an agent's config.toml does not have: {}",
            arm.name,
            unknown.join(", ")
        );
    }
    Ok(())
}

/// The arms `path` declares, the first being the baseline.
pub fn load(path: &Path) -> Result<Vec<Arm>> {
    let text =
        std::fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    parse(&text).wrap_err_with(|| format!("{}", path.display()))
}

pub fn parse(text: &str) -> Result<Vec<Arm>> {
    let file: ArmsFile = toml::from_str(text)?;
    check(&file.arm)?;
    Ok(file.arm)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOW_HOW: &str = r#"
        [[arm]]
        name = "current"
        agent = "analyst"

        [[arm]]
        name = "know-how-on"
        agent = "analyst-eval"
        copy_of = "analyst"
        [arm.overrides.know_how]
        enabled = true
        inject = "on"
    "#;

    #[test]
    fn the_first_arm_is_the_baseline_and_overrides_ride_a_copy() {
        let arms = parse(KNOW_HOW).unwrap();
        assert_eq!(arms[0], Arm::of_agent("analyst").named("current"));
        assert_eq!(arms[1].copy_of.as_deref(), Some("analyst"));
        assert_eq!(
            arms[1].overrides["know_how"]["inject"],
            Value::String("on".into())
        );
    }

    impl Arm {
        fn named(mut self, name: &str) -> Self {
            self.name = name.to_string();
            self
        }
    }

    #[test]
    fn overrides_are_refused_on_anything_but_a_copy() {
        for arms in [
            // No copy named.
            r#"[[arm]]
               name = "a"
               agent = "analyst"
               [arm.overrides.know_how]
               inject = "on""#,
            // The copy is itself.
            r#"[[arm]]
               name = "a"
               agent = "analyst"
               copy_of = "analyst"
               [arm.overrides.know_how]
               inject = "on""#,
            // The "copy" is another arm's real agent.
            r#"[[arm]]
               name = "a"
               agent = "analyst"
               [[arm]]
               name = "b"
               agent = "analyst"
               copy_of = "other"
               [arm.overrides.know_how]
               inject = "on""#,
            // The "copy" is another arm's original.
            r#"[[arm]]
               name = "a"
               agent = "copy"
               copy_of = "analyst"
               [[arm]]
               name = "b"
               agent = "analyst"
               copy_of = "other"
               [arm.overrides.know_how]
               inject = "on""#,
        ] {
            assert!(parse(arms).is_err(), "accepted:\n{arms}");
        }
    }

    #[test]
    fn only_an_agent_that_says_so_itself_is_a_test_copy() {
        assert!(is_test_copy("[evals]\ncopy = true\n"));
        for config in [
            "",
            "[evals]\ncopy = false\n",
            "[evals]\n",
            "[llm]\nmax_rounds = 30\n",
            "evals = true",
            "not toml [",
        ] {
            assert!(!is_test_copy(config), "{config:?}");
        }
        let claiming = KNOW_HOW.replace(
            "[arm.overrides.know_how]",
            "[arm.overrides.evals]\ncopy = true\n[arm.overrides.know_how]",
        );
        assert!(
            parse(&claiming).is_err(),
            "an arm marked its own agent a copy"
        );
    }

    #[test]
    fn an_override_the_config_would_drop_is_refused_by_name() {
        let misspelt = KNOW_HOW.replace("inject = \"on\"", "injekt = \"on\"");
        let refusal = format!("{:#}", parse(&misspelt).unwrap_err());
        assert!(refusal.contains("know_how.injekt"), "{refusal}");
        let unknown_section = KNOW_HOW.replace("overrides.know_how", "overrides.knowhow");
        assert!(parse(&unknown_section).is_err());
    }

    #[test]
    fn the_settings_arms_are_for_are_known() {
        for overrides in [
            "[arm.overrides.know_how]\nenabled = true\ninject = \"off\"\nmin_score = 0.6",
            "[arm.overrides.research]\nadversarial = true",
            "[arm.overrides.llm]\ndefault_model = \"cheap\"\nfailover = []",
            "[arm.overrides.llm.model_preferences]\ninvestigation = \"powerful\"",
            "[arm.overrides.claim_grounding]\nenabled = false",
            "[arm.overrides.tools.programs]\nenabled = true",
        ] {
            let arms = format!(
                "[[arm]]\nname = \"a\"\nagent = \"copy\"\ncopy_of = \"analyst\"\n{overrides}"
            );
            parse(&arms).unwrap_or_else(|e| panic!("{overrides}: {e:#}"));
        }
    }

    #[test]
    fn arm_names_are_unique() {
        let twice = KNOW_HOW.replace("name = \"know-how-on\"", "name = \"current\"");
        assert!(parse(&twice).is_err());
        assert!(parse("arm = []").is_err());
    }

    #[test]
    fn an_arms_hash_follows_its_agent_and_overrides() {
        let arms = parse(KNOW_HOW).unwrap();
        let shadow = parse(&KNOW_HOW.replace("inject = \"on\"", "inject = \"shadow\"")).unwrap();
        assert_ne!(arms[1].hash(), shadow[1].hash());
        assert_eq!(arms[0].hash(), shadow[0].hash());
        assert_ne!(arms[0].hash(), Arm::of_agent("other").hash());
    }

    #[test]
    fn overrides_lie_over_a_config_key_by_key() {
        let arm = &parse(KNOW_HOW).unwrap()[1];
        let merged: Table = arm
            .laid_over("[know_how]\nenabled = false\nmin_score = 0.6\n\n[llm]\nmax_rounds = 30\n")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(merged["know_how"]["enabled"], Value::Boolean(true));
        assert_eq!(merged["know_how"]["inject"], Value::String("on".into()));
        assert_eq!(merged["know_how"]["min_score"], Value::Float(0.6));
        assert_eq!(merged["llm"]["max_rounds"], Value::Integer(30));
        assert_eq!(
            arm.laid_over("").unwrap().parse::<Table>().unwrap(),
            arm.overrides
        );
    }
}
