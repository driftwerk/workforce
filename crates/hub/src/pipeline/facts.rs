//! What the workflows serving a pipeline depend on beyond its documents: the
//! build, the runner daemons and the protocol each announced, the skills
//! those workflows' tool steps call with their tools grants, and each
//! workflow with its schedules. Composed purely from what the hub gathers,
//! so a grant gone stale or a worker a hub bump left behind reads as data.

use std::collections::BTreeSet;

use serde_json::Value;
use workforce_common::settings::SecretStore;
use workforce_orchestrator::workflow::definition::WorkflowDefinition;
use workforce_wasm_runtime::InstalledWasmSkill;

use crate::hub_core::pipelines::WorkerFacts;
pub use crate::hub_core::pipelines::{Facts, Grant, ScheduleFacts, SkillFacts, WorkflowFacts};
use crate::hub_core::schedules::{JobPayload, Outcomes, Schedule, ScheduledJob};
use crate::skill_consent::{grant_status_of, shared_tools_grant_path, tools_grant_path};
use crate::wasm_skill_tools::source_commit;

/// How many of a schedule's latest runs its facts classify.
pub const LAST_RUNS: usize = 12;

/// A loaded workflow definition with the key a run addresses it by.
pub struct Workflow<'a> {
    pub key: &'a str,
    pub def: &'a WorkflowDefinition,
    pub content_hash: Option<&'a str>,
}

/// Everything [`facts`] composes.
pub struct Inputs<'a> {
    /// The pipeline's agent, on an agent's pipeline.
    pub agent: Option<&'a str>,
    /// The pipeline's `sources`, whose `workflow:<id>` entries name the
    /// workflows serving any other pipeline.
    pub sources: &'a [String],
    pub workflows: Vec<Workflow<'a>>,
    /// The installed skills the reader may run.
    pub skills: Vec<InstalledWasmSkill>,
    /// Every schedule with how its latest runs went.
    pub schedules: Vec<(ScheduledJob, Outcomes)>,
    pub workers: Vec<WorkerFacts>,
    pub build: Value,
}

/// The facts `inputs` hold for the workflows serving the pipeline, each
/// skill's tools grant read through `grant`.
pub fn facts(inputs: Inputs<'_>, grant: impl Fn(&InstalledWasmSkill) -> Option<Grant>) -> Facts {
    let Inputs {
        agent,
        sources,
        workflows,
        skills,
        schedules,
        workers,
        build,
    } = inputs;
    let mut serving: Vec<Workflow<'_>> = workflows
        .into_iter()
        .filter(|workflow| serves(agent, sources, workflow))
        .collect();
    serving.sort_by_key(|workflow| workflow.key);
    let called: BTreeSet<&str> = serving
        .iter()
        .flat_map(|workflow| tools_of(workflow.def))
        .collect();
    let required: BTreeSet<&str> = serving
        .iter()
        .flat_map(|workflow| workflow.def.workflow.requires.keys().map(String::as_str))
        .collect();
    let mut skills: Vec<SkillFacts> = skills
        .iter()
        .filter(|skill| {
            let prefix = format!("{}.", skill.manifest.name);
            required.contains(skill.manifest.name.as_str())
                || called.iter().any(|tool| tool.starts_with(&prefix))
        })
        .map(|skill| SkillFacts {
            name: skill.manifest.name.clone(),
            version: skill.manifest.version.clone(),
            source_commit: source_commit(&skill.source).map(str::to_string),
            sha256: skill.sha256.clone(),
            contract_fingerprint: skill.contract_fingerprint(),
            tools: skill
                .manifest
                .tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            grant: grant(skill),
        })
        .collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    let workflows = serving
        .iter()
        .map(|workflow| WorkflowFacts {
            id: workflow.key.to_string(),
            version: workflow.def.workflow.version.clone(),
            content_hash: workflow.content_hash.map(str::to_string),
            tools: tools_of(workflow.def).map(str::to_string).collect(),
            requires: workflow.def.workflow.requires.clone(),
            schedules: schedules
                .iter()
                .filter(|(job, _)| starts(job, workflow.key))
                .map(schedule_facts)
                .collect(),
        })
        .collect();
    Facts {
        build,
        workers,
        skills,
        workflows,
    }
}

/// `skill`'s tools grant for `identity` and for the org, read from `vault`;
/// `None` when the skill declares no platform tools.
pub fn grant_of(
    vault: &dyn SecretStore,
    skill: &InstalledWasmSkill,
    identity: Option<&str>,
) -> Option<Grant> {
    let name = skill.manifest.name.as_str();
    (!skill.manifest.permissions.is_empty()).then(|| {
        let status = grant_status_of(vault, skill);
        Grant {
            run_as: identity.map(|uid| status(&tools_grant_path(name, uid))),
            shared: status(&shared_tools_grant_path(name)),
        }
    })
}

fn serves(agent: Option<&str>, sources: &[String], workflow: &Workflow<'_>) -> bool {
    match agent {
        Some(agent) => workflow.def.workflow.agent.as_deref() == Some(agent),
        None => sources
            .iter()
            .filter_map(|source| source.strip_prefix("workflow:"))
            .any(|id| id == workflow.def.workflow.id || id == workflow.key),
    }
}

fn tools_of(def: &WorkflowDefinition) -> impl Iterator<Item = &str> {
    def.steps
        .iter()
        .filter_map(|step| step.tool.as_deref())
        .collect::<BTreeSet<_>>()
        .into_iter()
}

fn starts(job: &ScheduledJob, key: &str) -> bool {
    matches!(&job.payload, JobPayload::WorkflowStart { workflow_id, .. } if workflow_id == key)
}

fn schedule_facts((job, last_12): &(ScheduledJob, Outcomes)) -> ScheduleFacts {
    let (cron, tz) = match &job.schedule {
        Schedule::Cron { expr, tz } => (Some(expr.clone()), tz.clone()),
        Schedule::At { .. } | Schedule::Every { .. } => (None, None),
    };
    ScheduleFacts {
        job_id: job.id.clone(),
        cron,
        tz,
        enabled: job.enabled,
        next_run: job.next_run,
        last_run: job.last_run,
        last_12: *last_12,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use workforce_wasm_runtime::SkillManifest;

    use super::*;
    use crate::hub_core::grants::GrantStatus;
    use crate::skill_consent::tests::{installed_fixture, MapVault, ENABLER};

    const PINNED: &str = "4d44c19411b2c3e5f60718293a4b5c6d7e8f9012";

    fn workflow(id: &str, agent: &str, tools: &[&str]) -> WorkflowDefinition {
        let steps: String = tools
            .iter()
            .enumerate()
            .map(|(i, tool)| {
                let end = if i + 1 == tools.len() {
                    "exit = true".to_string()
                } else {
                    format!("goto = \"s{}\"", i + 1)
                };
                format!("[[step]]\nid = \"s{i}\"\nname = \"S{i}\"\ntool = \"{tool}\"\ntask = \"{{}}\"\n{end}\n\n")
            })
            .collect();
        WorkflowDefinition::from_toml(&format!(
            "[workflow]\nid = \"{id}\"\nname = \"{id}\"\nversion = \"1.2\"\nagent = \"{agent}\"\n\n{steps}"
        ))
        .unwrap()
    }

    fn enabler() -> InstalledWasmSkill {
        InstalledWasmSkill {
            manifest: SkillManifest::from_toml(ENABLER).unwrap(),
            source: format!("github:acme/skills@{PINNED}#subpath=workforce/wasm/enabler"),
            ..installed_fixture("enabler", None)
        }
    }

    fn job(id: &str, workflow_id: &str) -> ScheduledJob {
        serde_json::from_value(json!({
            "id": id,
            "name": format!("workflow:{workflow_id}:0"),
            "schedule": {"kind": "cron", "expr": "9-59/15 * * * *", "tz": "Europe/London"},
            "payload": {"kind": "workflow_start", "workflow_id": workflow_id},
            "enabled": true,
            "created_at": "2026-09-15T04:00:00Z",
            "last_run": "2026-09-15T04:39:00Z",
            "next_run": "2026-09-15T04:54:00Z",
            "run_count": 12
        }))
        .unwrap()
    }

    fn outcomes(started: usize, skipped: usize, failed: usize) -> Outcomes {
        Outcomes {
            considered: started + skipped + failed,
            started,
            skipped,
            failed,
        }
    }

    struct Hub {
        ticket_to_pr: WorkflowDefinition,
        pr_review: WorkflowDefinition,
        triage: WorkflowDefinition,
    }

    impl Hub {
        fn new() -> Self {
            Self {
                ticket_to_pr: workflow(
                    "order-intake",
                    "example",
                    &[
                        "item-picker.claim_next",
                        "pipeline.record",
                        "item-picker.claim_next",
                    ],
                ),
                pr_review: workflow("order-review", "example", &["enabler.ping"]),
                triage: workflow("hubspot-triage", "quill", &["hubspot-helper.pick"]),
            }
        }

        fn inputs<'a>(&'a self, agent: Option<&'a str>, sources: &'a [String]) -> Inputs<'a> {
            Inputs {
                agent,
                sources,
                workflows: vec![
                    Workflow {
                        key: "order-intake",
                        def: &self.ticket_to_pr,
                        content_hash: Some("b3-ticket"),
                    },
                    Workflow {
                        key: "69116b82/order-review",
                        def: &self.pr_review,
                        content_hash: None,
                    },
                    Workflow {
                        key: "hubspot-triage",
                        def: &self.triage,
                        content_hash: None,
                    },
                ],
                skills: vec![
                    installed_fixture("item-picker", None),
                    enabler(),
                    installed_fixture("hubspot-helper", None),
                ],
                schedules: vec![
                    (job("job-ticket", "order-intake"), outcomes(9, 2, 1)),
                    (job("job-triage", "hubspot-triage"), outcomes(12, 0, 0)),
                ],
                workers: Vec::new(),
                build: json!({"version": "0.1.0", "commit_sha": "dev"}),
            }
        }
    }

    fn record(skill: &InstalledWasmSkill, contract: &str) -> String {
        json!({
            "fingerprint": skill.grant_fingerprint().unwrap(),
            "contract_fingerprint": contract,
        })
        .to_string()
    }

    #[test]
    fn an_agents_pipeline_reads_its_workflows_their_skills_and_their_schedules_only() {
        let hub = Hub::new();
        let facts = facts(hub.inputs(Some("example"), &[]), |_| None);
        let ids: Vec<&str> = facts.workflows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["69116b82/order-review", "order-intake"]);
        let ticket = &facts.workflows[1];
        assert_eq!(ticket.version, "1.2");
        assert_eq!(ticket.content_hash.as_deref(), Some("b3-ticket"));
        assert_eq!(ticket.tools, ["item-picker.claim_next", "pipeline.record"]);
        assert_eq!(ticket.schedules.len(), 1);
        let schedule = &ticket.schedules[0];
        assert_eq!(schedule.job_id, "job-ticket");
        assert_eq!(schedule.cron.as_deref(), Some("9-59/15 * * * *"));
        assert_eq!(schedule.tz.as_deref(), Some("Europe/London"));
        assert!(schedule.enabled);
        assert_eq!(schedule.last_12, outcomes(9, 2, 1));
        assert!(facts.workflows[0].schedules.is_empty());

        let skills: Vec<&str> = facts.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            skills,
            ["enabler", "item-picker"],
            "a skill only another agent's workflow calls is left out"
        );
        let pinned = &facts.skills[0];
        assert_eq!(pinned.source_commit.as_deref(), Some(PINNED));
        assert_eq!(pinned.tools, ["ping"]);
        assert_eq!(
            pinned.contract_fingerprint,
            enabler().contract_fingerprint()
        );
        assert_eq!(facts.skills[1].source_commit, None);
        assert_eq!(facts.build["commit_sha"], "dev");
    }

    #[test]
    fn any_other_pipeline_reads_the_workflows_its_sources_name() {
        let hub = Hub::new();
        let sources = [
            "workflow:hubspot-triage".to_string(),
            "skill:item-picker".to_string(),
        ];
        let facts = facts(hub.inputs(None, &sources), |_| None);
        let ids: Vec<&str> = facts.workflows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["hubspot-triage"]);
        assert_eq!(
            facts
                .skills
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            ["hubspot-helper"]
        );
        assert_eq!(facts.workflows[0].schedules[0].last_12, outcomes(12, 0, 0));

        let by_key = ["workflow:69116b82/order-review".to_string()];
        let facts = super::facts(hub.inputs(None, &by_key), |_| None);
        assert_eq!(facts.workflows[0].id, "69116b82/order-review");
        assert!(super::facts(hub.inputs(None, &[]), |_| None)
            .workflows
            .is_empty());
    }

    #[test]
    fn a_workflow_carries_its_requires_and_a_skill_it_requires_is_read_though_no_step_calls_it() {
        let mut hub = Hub::new();
        hub.pr_review.workflow.requires = BTreeMap::from([
            ("hubspot-helper".to_string(), ">=0.2.0".to_string()),
            ("enabler".to_string(), ">=0.1.0".to_string()),
        ]);
        let facts = facts(hub.inputs(Some("example"), &[]), |_| None);
        let skills: Vec<&str> = facts.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(skills, ["enabler", "hubspot-helper", "item-picker"]);
        assert_eq!(facts.workflows[0].requires, hub.pr_review.workflow.requires);
        assert!(facts.workflows[1].requires.is_empty());
        let json = serde_json::to_value(&facts).unwrap();
        assert_eq!(
            json["workflows"][0]["requires"],
            json!({"enabler": ">=0.1.0", "hubspot-helper": ">=0.2.0"})
        );
        assert_eq!(json["workflows"][1]["requires"], json!({}));
    }

    #[test]
    fn grants_read_valid_stale_or_missing_for_the_identity_and_the_org() {
        let skill = enabler();
        let vault = MapVault::default();
        vault
            .set(
                &tools_grant_path("enabler", "alice"),
                &record(&skill, &skill.contract_fingerprint()),
            )
            .unwrap();
        vault
            .set(
                &shared_tools_grant_path("enabler"),
                &record(&skill, "an-older-contract"),
            )
            .unwrap();
        assert_eq!(
            grant_of(&vault, &skill, Some("alice")),
            Some(Grant {
                run_as: Some(GrantStatus::Valid),
                shared: GrantStatus::Stale
            })
        );
        assert_eq!(
            grant_of(&vault, &skill, Some("bob")),
            Some(Grant {
                run_as: Some(GrantStatus::Missing),
                shared: GrantStatus::Stale
            })
        );
        assert_eq!(
            grant_of(&vault, &skill, None).map(|grant| grant.run_as),
            Some(None)
        );
        assert_eq!(
            grant_of(
                &vault,
                &installed_fixture("item-picker", None),
                Some("alice")
            ),
            None
        );

        let hub = Hub::new();
        let facts = facts(hub.inputs(Some("example"), &[]), |skill| {
            grant_of(&vault, skill, Some("alice"))
        });
        let json = serde_json::to_value(&facts).unwrap();
        assert_eq!(
            json["skills"][0]["grant"],
            json!({"run_as": "valid", "shared": "stale"})
        );
        assert_eq!(json["skills"][1]["grant"], Value::Null);
        assert_eq!(
            json["workflows"][1]["schedules"][0]["last_12"],
            json!({"considered": 12, "started": 9, "skipped": 2, "failed": 1})
        );
    }
}
