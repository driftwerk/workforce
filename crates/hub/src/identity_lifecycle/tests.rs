use crate::serve::Compose as _;
use std::sync::Arc;

use chrono::Utc;
use workforce_identity::lifecycle::{LifecycleOpStore, OpStatus};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::{IdentityAndTeamStore, IdentityStore};
use workforce_identity::team::TeamStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_memory::types::{Memory, MemoryLayer};

use super::*;
use crate::agent_registry::AgentRegistry;
use crate::hub_core::schedules::{JobPayload, Schedule};

pub(super) struct Hub {
    pub(super) lifecycle: Arc<Lifecycle>,
    pub(super) service: Arc<AgentService>,
    pub(super) identities: Arc<SqliteIdentityStore>,
    pub(super) _dir: tempfile::TempDir,
}

pub(super) async fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = dir.path().to_string_lossy().to_string();
    config.memory.db_path = dir.path().join("memory.db").to_string_lossy().into_owned();
    config.llm.api_key = Some("test-key".into());
    let (service, _) = AgentService::from_config_with_scheduler(config, None)
        .await
        .unwrap();
    let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let lifecycle = Arc::new(Lifecycle {
        registry: Arc::new(AgentRegistry::single(Arc::clone(&service))),
        identities: Arc::clone(&identities) as Arc<dyn IdentityAndTeamStore>,
        devices: None,
        channels: None,
        defs: None,
        artifacts: None,
        pictures: None,
        external_skills: crate::external_skills::ExternalSkills::shared(&dir.path().join("skills")),
        data_dir: dir.path().to_path_buf(),
        runs: Runs::default(),
    });
    Hub {
        lifecycle,
        service,
        identities,
        _dir: dir,
    }
}

impl Hub {
    pub(super) fn person(&self, name: &str, slack: &str) -> Identity {
        let identity = Identity::new(name, IdentityRole::Member).with_linked_id("slack", slack);
        self.identities.create(&identity).unwrap();
        identity
    }

    pub(super) fn admin(&self) -> Actor {
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        self.identities.create(&admin).unwrap();
        Actor(Some(admin.id))
    }

    pub(super) fn scheduler(&self) -> Arc<dyn crate::hosts::SchedulerHost> {
        self.service.scheduler.clone().expect("a scheduler")
    }

    pub(super) async fn job_for(&self, who: &Identity) -> String {
        let scheduler = self.scheduler();
        let id = scheduler
            .add_job_with_desc(
                "digest",
                None,
                Schedule::Every {
                    every_secs: 3600,
                    anchor: None,
                },
                JobPayload::Reminder {
                    text: "stretch".into(),
                    user_id: who.id.to_string(),
                },
            )
            .await
            .unwrap();
        scheduler
            .update_job_identity(&id, Some(who.id.to_string()))
            .await;
        id
    }

    pub(super) async fn memory_of(
        &self,
        who: &Identity,
        content: &str,
    ) -> workforce_memory::types::MemoryId {
        let mut memory = Memory::new(
            content,
            MemoryLayer::Episodic,
            self.service.agent_id.clone(),
        );
        memory.identity_id = Some(who.id.to_string());
        self.memories().remember(memory).await.unwrap()
    }

    pub(super) fn memories(&self) -> Arc<dyn workforce_memory::store::MemoryStore> {
        self.service.memory_store.clone().expect("a memory store")
    }

    pub(super) fn notes(&self, who: &Identity) -> std::path::PathBuf {
        self.service
            .workspace
            .join("memory")
            .join("users")
            .join(who.id.to_string())
    }

    pub(super) fn write_note(&self, who: &Identity, file: &str, body: &str) {
        let dir = self.notes(who);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    /// The lifecycle, before anything shares it.
    pub(super) fn lifecycle_mut(&mut self) -> &mut Lifecycle {
        Arc::get_mut(&mut self.lifecycle).expect("not shared yet")
    }

    /// A relationship event, a lesson event and a described cost row
    /// naming `name`, beside billing that pays for it.
    pub(super) fn signals_for(&self, name: &str) {
        let telemetry = &self.service.telemetry_store;
        let agent = self.service.agent_id.as_str().to_string();
        telemetry
            .record_relationship_event(&workforce_common::telemetry::RelationshipEvent {
                timestamp: Utc::now(),
                agent_id: agent.clone(),
                identity_id: name.into(),
                session_id: format!("s-{name}"),
                message_id: "m1".into(),
                valence: 0.5,
                intensity: 0.5,
                emotion_label: None,
                reaction: "praise".into(),
                is_key_moment: false,
                confidence: 0.9,
                source: "code".into(),
                signals: "{}".into(),
            })
            .unwrap();
        telemetry
            .record_lesson_event(
                &workforce_common::telemetry::LessonEvent::new(Utc::now(), &agent, "injected")
                    .with_identity(name),
            )
            .unwrap();
        telemetry
            .record_cost(&workforce_common::persistence::CostRecord {
                agent_id: agent,
                model: "m".into(),
                cost_usd: 1.0,
                identity_id: Some(name.into()),
                description: Some("summarise my letters".into()),
                payer: Some(name.into()),
                ..Default::default()
            })
            .unwrap();
    }

    pub(super) fn category(report: &Report, category: Category) -> &CategoryReport {
        report
            .categories
            .iter()
            .find(|c| c.category == category)
            .expect("every category reports")
    }
}

/// A delete of `subject` another replica runs, its lease live for minutes.
pub(super) fn running_elsewhere(subject: Uuid) -> workforce_identity::lifecycle::LifecycleOp {
    workforce_identity::lifecycle::LifecycleOp {
        id: Uuid::new_v4(),
        subject,
        kind: "delete".into(),
        target: None,
        actor: "elsewhere".into(),
        names: vec![],
        status: OpStatus::Running,
        report: serde_json::json!({}),
        started_at: Utc::now(),
        lease_until: Utc::now() + Duration::minutes(5),
        finished_at: None,
    }
}

#[test]
fn every_category_is_visited_once_and_named_as_it_serialises() {
    let mut seen = std::collections::HashSet::new();
    for category in Category::ALL {
        assert!(seen.insert(category), "{category:?} twice");
        assert_eq!(
            serde_json::to_value(category).unwrap(),
            serde_json::json!(category.name())
        );
        let _ = holding(category);
    }
    assert_eq!(
        Category::ALL[0],
        Category::IdentityRecord,
        "the identity's links move first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_merge_reports_everything_and_writes_nothing() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let job = hub.job_for(&alice).await;
    hub.memory_of(&alice, "alice likes tea").await;
    hub.service
        .sessions
        .get_or_create("dm", &alice.id.to_string(), hub.service.agent_id.as_str())
        .unwrap();

    let report = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, true)
        .await
        .unwrap();

    assert!(report.dry_run && !report.complete);
    for category in [
        Category::ScheduledJobs,
        Category::Memories,
        Category::Sessions,
    ] {
        assert_eq!(
            Hub::category(&report, category).tally.count,
            1,
            "{category:?}"
        );
    }
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_some());
    assert_eq!(
        hub.scheduler()
            .get_job(&job)
            .await
            .unwrap()
            .creator_identity_id,
        Some(alice.id.to_string())
    );
    assert!(hub
        .lifecycle
        .operations(Some(alice.id), 10)
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_moves_everything_unions_notes_and_retires_the_source() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let job = hub.job_for(&alice).await;
    let memory = hub.memory_of(&alice, "alice likes tea").await;
    let forgotten = hub.memory_of(&alice, "alice moved").await;
    hub.memories().forget(&forgotten).await.unwrap();
    let agent = hub.service.agent_id.as_str().to_string();
    let sessions = &hub.service.sessions;
    let alices = sessions
        .get_or_create("dm", &alice.id.to_string(), &agent)
        .unwrap();
    let bobs = sessions
        .get_or_create("dm", &bob.id.to_string(), &agent)
        .unwrap();
    hub.write_note(&alice, "2026-10-01.md", "alice's day");
    hub.write_note(&bob, "2026-10-01.md", "bob's day");
    hub.write_note(&alice, "profile.md", "alice's profile");
    hub.write_note(&alice, "active-task.meta.json", "{}");
    hub.write_note(&bob, "active-task.meta.json", "{\"bob\": true}");

    let report = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    for category in [
        Category::ScheduledJobs,
        Category::Memories,
        Category::Sessions,
    ] {
        assert!(
            Hub::category(&report, category).tally.count > 0,
            "{category:?} reports what moved"
        );
    }
    assert!(
        hub.identities.get_by_id(alice.id).unwrap().is_none(),
        "retired"
    );
    let bob_now = hub.identities.get_by_id(bob.id).unwrap().unwrap();
    assert_eq!(
        bob_now.linked_ids.get("slack").map(String::as_str),
        Some("U-B")
    );
    assert!(Hub::category(&report, Category::IdentityRecord)
        .tally
        .conflicts
        .iter()
        .any(|c| c.starts_with("slack")));

    let moved = hub.scheduler().get_job(&job).await.unwrap();
    assert_eq!(moved.creator_identity_id, Some(bob.id.to_string()));
    let mine = hub
        .memories()
        .identity_memories(&hub.service.agent_id, &bob.id.to_string())
        .await
        .unwrap();
    assert!(
        mine.contains(&memory) && mine.contains(&forgotten),
        "memories union"
    );

    assert_eq!(sessions.get(&bobs.id).unwrap().unwrap().channel, "dm");
    let kept = sessions.get(&alices.id).unwrap().unwrap();
    assert_eq!(kept.owner, bob.id.to_string());
    assert_eq!(
        kept.channel,
        workforce_common::session::reassigned_channel("dm", &Person::of(&alice).tag()),
        "both conversations are kept, the merged one renamed"
    );

    let notes = hub.notes(&bob);
    let day = std::fs::read_to_string(notes.join("2026-10-01.md")).unwrap();
    assert!(
        day.starts_with("bob's day") && day.contains("alice's day"),
        "{day}"
    );
    assert_eq!(
        std::fs::read_to_string(notes.join("profile.md")).unwrap(),
        "alice's profile"
    );
    assert_eq!(
        std::fs::read_to_string(notes.join("active-task.meta.json")).unwrap(),
        "{\"bob\": true}",
        "a structured file both have keeps the target's"
    );
    assert!(!hub.notes(&alice).exists());

    let ops = hub.lifecycle.operations(Some(alice.id), 10).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].status, OpStatus::Complete);
    assert_eq!(ops[0].target, Some(bob.id));
    assert_eq!(ops[0].actor, admin.name());
    assert_eq!(
        ops[0].report["categories"].as_array().unwrap().len(),
        Category::ALL.len()
    );
}

/// What the signals category counts is what a merge changes, and the
/// person's billing goes with them: nothing is left under any name of the
/// merged identity, and the amounts are unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_moves_every_signal_it_counts_and_leaves_none_behind() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let names = Person::of(&alice).names;
    for name in &names {
        hub.signals_for(name);
    }
    hub.service
        .telemetry_store
        .record_cost(&workforce_common::persistence::CostRecord {
            agent_id: hub.service.agent_id.as_str().to_string(),
            model: "m".into(),
            cost_usd: 0.5,
            identity_id: Some(alice.id.to_string()),
            payer: Some(alice.id.to_string()),
            ..Default::default()
        })
        .unwrap();
    let cx = hub.lifecycle.context(&admin);
    let held = holding(Category::Signals)
        .held(&cx, &Person::of(&alice))
        .await
        .unwrap()
        .count;
    assert_eq!(held, 3 * names.len());

    let report = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    assert_eq!(
        Hub::category(&report, Category::Signals).tally.count,
        held,
        "the report counts the rows changed"
    );
    let owners = holding(Category::Signals).owners(&cx).await.unwrap();
    assert_eq!(
        owners.held,
        BTreeMap::from([(bob.id.to_string(), held)]),
        "everything counted is bob's, nothing left under alice's names"
    );
    let spent = hub
        .service
        .telemetry_store
        .cost_by_identity(chrono::DateTime::<Utc>::UNIX_EPOCH, Utc::now())
        .unwrap();
    assert_eq!(
        spent,
        [(bob.id.to_string(), names.len() as f64 + 0.5)],
        "alice's spend is bob's now, every cent of it"
    );
    let payers: BTreeSet<_> = hub
        .service
        .telemetry_store
        .cost_split(
            chrono::DateTime::<Utc>::UNIX_EPOCH,
            Utc::now(),
            Default::default(),
        )
        .unwrap()
        .into_iter()
        .map(|row| row.payer)
        .collect();
    assert_eq!(payers, BTreeSet::from([Some(bob.id.to_string())]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_of_a_run_that_changed_meanwhile_is_left_for_the_next_pass() {
    let hub = hub().await;
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    let alice = uuid::Uuid::new_v4().to_string();
    let bob = uuid::Uuid::new_v4().to_string();
    telemetry
        .save_workflow_instance(&run(
            "run-1",
            &format!("{alice}/digest"),
            "running",
            serde_json::json!({ "_run_as": alice }),
        ))
        .unwrap();
    let read = telemetry.get_workflow_instance("run-1").unwrap().unwrap();
    telemetry
        .update_workflow_instance_status("run-1", "paused", Some("review"))
        .unwrap();

    let left = automation::merge_run(telemetry.as_ref(), read, std::slice::from_ref(&alice), &bob)
        .unwrap();

    assert_eq!(left.as_deref(), Some("run run-1 changed; try again"));
    let kept = telemetry.get_workflow_instance("run-1").unwrap().unwrap();
    assert_eq!(
        (kept.status.as_str(), kept.workflow_id),
        ("paused", format!("{alice}/digest"))
    );
    assert_eq!(kept.owner_instance, None, "its lease is let go");

    let fresh = telemetry.get_workflow_instance("run-1").unwrap().unwrap();
    assert_eq!(
        automation::merge_run(telemetry.as_ref(), fresh, &[alice], &bob).unwrap(),
        None
    );
    assert_eq!(
        telemetry
            .get_workflow_instance("run-1")
            .unwrap()
            .unwrap()
            .workflow_id,
        format!("{bob}/digest")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_merge_resumes_with_the_names_it_recorded() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B2"));
    let admin = hub.admin();
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    let now = Utc::now().to_rfc3339();
    telemetry
        .save_workflow_instance(&workforce_common::persistence::WorkflowInstanceRow {
            id: "run-1".into(),
            workflow_id: format!("{}/digest", alice.id),
            status: "running".into(),
            step_history: "[]".into(),
            variables: serde_json::json!({ "_run_as": alice.id.to_string() }).to_string(),
            trigger_json: "{}".into(),
            created_at: now.clone(),
            updated_at: now,
            ..Default::default()
        })
        .unwrap();
    assert!(telemetry
        .try_claim_workflow_instance("run-1", "another-replica", 600)
        .unwrap());
    hub.service
        .sessions
        .get_or_create("slack-dm", "U-A", hub.service.agent_id.as_str())
        .unwrap();

    let first = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();
    assert!(!first.complete);
    assert_eq!(
        Hub::category(&first, Category::WorkflowRuns)
            .tally
            .blocking
            .len(),
        1,
        "a run another replica is executing is left for the next pass"
    );
    let source = hub
        .identities
        .get_by_id(alice.id)
        .unwrap()
        .expect("kept to resume");
    assert!(source.linked_ids.is_empty(), "its links moved first");
    assert_eq!(
        hub.identities
            .get_by_id(bob.id)
            .unwrap()
            .unwrap()
            .linked_ids["slack"],
        "U-B2",
        "the target keeps its own link on a clash"
    );

    telemetry
        .release_workflow_instance("run-1", "another-replica")
        .unwrap();
    let second = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();

    assert!(second.complete, "{second:#?}");
    let run = telemetry.get_workflow_instance("run-1").unwrap().unwrap();
    assert_eq!(run.workflow_id, format!("{}/digest", bob.id));
    assert!(run.variables.contains(&bob.id.to_string()));
    assert!(
        hub.service
            .sessions
            .list_for_owner("U-A", 10)
            .unwrap()
            .is_empty(),
        "the bare platform id it was recorded under before is still reached"
    );
    let ops = hub.lifecycle.operations(Some(alice.id), 10).unwrap();
    assert_eq!(
        ops.iter().map(|op| op.status).collect::<Vec<_>>(),
        [OpStatus::Complete, OpStatus::Incomplete]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_lease_refuses_a_second_operation_on_the_same_identity() {
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let mut held = running_elsewhere(alice.id);
    assert!(hub.identities.claim_op(&held, Utc::now()).unwrap());

    let refused = hub.lifecycle.merge(alice.id, bob.id, &admin, false).await;
    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_some());

    held.lease_until = Utc::now();
    hub.identities
        .renew_op(held.id, held.lease_until, &held.report)
        .unwrap();
    assert!(
        hub.lifecycle
            .merge(alice.id, bob.id, &admin, false)
            .await
            .unwrap()
            .complete
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_erases_everything_personal_and_keeps_billing() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let job = hub.job_for(&alice).await;
    hub.memory_of(&alice, "alice likes tea").await;
    let forgotten = hub.memory_of(&alice, "alice moved").await;
    hub.memories().forget(&forgotten).await.unwrap();
    hub.write_note(&alice, "2026-10-01.md", "alice's day");
    let session = hub
        .service
        .sessions
        .get_or_create("dm", &alice.id.to_string(), hub.service.agent_id.as_str())
        .unwrap();
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    telemetry
        .record_cost(&workforce_common::persistence::CostRecord {
            agent_id: "a".into(),
            model: "m".into(),
            cost_usd: 2.0,
            identity_id: Some(alice.id.to_string()),
            description: Some("summarise my letters".into()),
            ..Default::default()
        })
        .unwrap();

    let preview = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, true)
        .await
        .unwrap();
    assert_eq!(Hub::category(&preview, Category::Sessions).tally.count, 1);
    assert_eq!(
        Hub::category(&preview, Category::ScheduledJobs).choice,
        Some(Choice::Erase)
    );
    assert!(
        hub.scheduler().get_job(&job).await.is_some(),
        "a dry run writes nothing"
    );

    let report = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    assert!(hub.identities.get_by_id(alice.id).unwrap().is_none());
    assert!(hub.scheduler().get_job(&job).await.is_none());
    assert!(hub.service.sessions.get(&session.id).unwrap().is_none());
    assert!(hub
        .memories()
        .identity_memories(&hub.service.agent_id, &alice.id.to_string())
        .await
        .unwrap()
        .is_empty());
    assert!(!hub.notes(&alice).exists());
    assert_eq!(
        telemetry
            .identity_signals(&alice.id.to_string())
            .unwrap()
            .total(),
        0
    );
    let spent = telemetry
        .cost_by_identity(chrono::DateTime::<Utc>::UNIX_EPOCH, Utc::now())
        .unwrap();
    assert!(
        spent.contains(&(alice.id.to_string(), 2.0)),
        "billing is kept: {spent:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_refuses_to_strand_shared_items_without_a_recipient() {
    use workforce_orchestrator::artifact_store::{
        Artifact, ArtifactBackend, ArtifactType, Caller, FsArtifactBackend, Visibility,
    };
    let hub = hub().await;
    let (alice, carol) = (hub.person("Alice", "U-A"), hub.person("Carol", "U-C"));
    let admin = hub.admin();
    let artifacts = FsArtifactBackend::create_at(&hub.service.workspace).unwrap();
    for (id, visibility) in [
        ("shared", Visibility::Public),
        ("private", Visibility::Private),
    ] {
        artifacts
            .create(&Artifact::new(
                id,
                ArtifactType::Snippet,
                id,
                "content",
                alice.id.to_string(),
                alice.id.to_string(),
                visibility,
                vec![],
            ))
            .await
            .unwrap();
    }

    let refused = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, true)
        .await;
    assert!(
        matches!(&refused, Err(LifecycleError::Refused(why)) if why.contains("artifacts (1)")),
        "{refused:?}"
    );

    let plan = DeletePlan {
        recipient: Some(carol.id),
        choices: BTreeMap::new(),
    };
    let report = hub
        .lifecycle
        .delete(alice.id, &plan, &admin, false)
        .await
        .unwrap();
    assert!(report.complete, "{report:#?}");
    let shared = artifacts
        .read("shared", &Caller::admin())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shared.owner_id, carol.id.to_string());
    assert!(
        artifacts
            .read("private", &Caller::admin())
            .await
            .unwrap()
            .is_none(),
        "a private item is erased, never handed to someone else"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transferred_job_counts_as_edited_by_the_admin_who_moved_it() {
    let hub = hub().await;
    let (alice, carol) = (hub.person("Alice", "U-A"), hub.person("Carol", "U-C"));
    let admin = hub.admin();
    let job = hub.job_for(&alice).await;
    let item = Item::Job {
        agent: hub.service.agent_id.as_str().to_string(),
        id: job.clone(),
    };

    let report = hub
        .lifecycle
        .transfer(alice.id, &item, carol.id, &admin)
        .await
        .unwrap();

    assert!(report.complete);
    let moved = hub.scheduler().get_job(&job).await.unwrap();
    assert_eq!(moved.creator_identity_id, Some(carol.id.to_string()));
    assert_eq!(moved.edited_by, Some(admin.name()));
    assert!(
        !crate::scheduler::is_own(&moved),
        "it runs without the new owner's private context until they save it"
    );
    assert_eq!(
        hub.lifecycle.operations(Some(alice.id), 1).unwrap()[0].kind,
        "transfer"
    );
    let missing = Item::Job {
        agent: hub.service.agent_id.as_str().to_string(),
        id: "nope".into(),
    };
    assert!(matches!(
        hub.lifecycle
            .transfer(alice.id, &missing, carol.id, &admin)
            .await,
        Err(LifecycleError::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_admin_cannot_merge_away_or_delete_themselves_or_merge_into_themselves() {
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let admin = hub.admin();
    let me = admin.0.unwrap();
    for refused in [
        hub.lifecycle.merge(me, alice.id, &admin, true).await,
        hub.lifecycle
            .delete(me, &DeletePlan::default(), &admin, true)
            .await,
        hub.lifecycle.merge(alice.id, alice.id, &admin, true).await,
    ] {
        assert!(
            matches!(refused, Err(LifecycleError::Refused(_))),
            "{refused:?}"
        );
    }
}

#[test]
fn a_vault_merge_moves_entries_keeps_the_targets_on_a_clash_and_drops_sign_ins() {
    let dir = tempfile::tempdir().unwrap();
    let vault = crate::vault::Vault::open(
        &dir.path().join("vault.enc"),
        &crate::vault::Vault::generate_master_key(),
    )
    .unwrap();
    let alice = Identity::new("Alice", IdentityRole::Member).with_linked_id("slack", "U-A");
    let bob = Identity::new("Bob", IdentityRole::Member);
    let (a, b) = (alice.id.to_string(), bob.id.to_string());
    for (path, value) in [
        (format!("users/{a}/github/token"), "a-gh"),
        (format!("users/{a}/codex/auth_json"), "a-codex"),
        (format!("users/{b}/codex/auth_json"), "b-codex"),
        (
            format!("wasm-skills/notes/grants/users/{a}/_tools"),
            "granted",
        ),
        (format!("subscription/device_login/{a}"), "in-flight"),
        ("shared/key".to_string(), "shared"),
    ] {
        vault.set(&path, value).unwrap();
    }
    let (from, into) = (Person::of(&alice), Person::of(&bob));

    let preview = super::records::merge_in(&vault, "global", &from, &into, false).unwrap();
    assert_eq!(preview.count, 4);
    assert_eq!(vault.list(None).len(), 6, "a dry run writes nothing");

    let tally = super::records::merge_in(&vault, "global", &from, &into, true).unwrap();
    assert_eq!(
        tally.conflicts,
        [format!(
            "global: users/{b}/codex/auth_json kept; the merged copy was dropped"
        )]
    );
    assert_eq!(
        vault.get(&format!("users/{b}/github/token")).as_deref(),
        Some("a-gh")
    );
    assert_eq!(
        vault.get(&format!("users/{b}/codex/auth_json")).as_deref(),
        Some("b-codex")
    );
    assert_eq!(
        vault
            .get(&format!("wasm-skills/notes/grants/users/{b}/_tools"))
            .as_deref(),
        Some("granted")
    );
    assert!(vault.list(Some(&format!("users/{a}/"))).is_empty());
    assert!(vault
        .get(&format!("subscription/device_login/{a}"))
        .is_none());
    assert_eq!(vault.get("shared/key").as_deref(), Some("shared"));

    let erased = super::records::delete_in(&vault, "global", &into, true).unwrap();
    assert_eq!(erased.count, 3);
    assert_eq!(vault.list(None), ["shared/key"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delete_moves_only_the_persons_own_jobs_and_releases_others_that_name_them() {
    let hub = hub().await;
    let (alice, bob, carol, dave) = (
        hub.person("Alice", "U-A"),
        hub.person("Bob", "U-B"),
        hub.person("Carol", "U-C"),
        hub.person("Dave", "U-D"),
    );
    let admin = hub.admin();
    let scheduler = hub.scheduler();
    let alices = hub.job_for(&alice).await;
    let carols = hub.job_for(&carol).await;
    scheduler
        .update_job_bill_to(&carols, Some(alice.id.to_string()))
        .await;
    let daves = hub.job_for(&dave).await;
    let plan = DeletePlan {
        recipient: Some(bob.id),
        choices: BTreeMap::from([(Category::ScheduledJobs, Choice::Transfer)]),
    };

    let preview = hub
        .lifecycle
        .delete(alice.id, &plan, &admin, true)
        .await
        .unwrap();
    let jobs = &Hub::category(&preview, Category::ScheduledJobs).tally;
    assert_eq!(
        jobs.count, 2,
        "their own job, and one of another's that bills them"
    );
    assert_eq!(
        jobs.movable.len(),
        1,
        "only their own job is theirs to hand over"
    );

    assert!(
        hub.lifecycle
            .delete(alice.id, &plan, &admin, false)
            .await
            .unwrap()
            .complete
    );

    let moved = scheduler.get_job(&alices).await.unwrap();
    assert_eq!(moved.creator_identity_id, Some(bob.id.to_string()));
    assert_eq!(moved.edited_by, Some(admin.name()));
    let kept = scheduler
        .get_job(&carols)
        .await
        .expect("another's job survives");
    assert_eq!(kept.creator_identity_id, Some(carol.id.to_string()));
    assert_eq!(kept.bill_to_identity_id, None, "it bills its owner again");
    assert_eq!(kept.edited_by, None);
    let untouched = scheduler.get_job(&daves).await.unwrap();
    assert_eq!(
        untouched.edited_by, None,
        "a job naming nobody deleted is not stamped"
    );
    assert!(crate::scheduler::is_own(&untouched));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_revokes_a_device_the_target_already_has_one_of() {
    use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};
    let mut hub = hub().await;
    let devices = Arc::new(DevicePairingService::new(
        Arc::new(SqlitePairingStore::in_memory().unwrap()),
        PairingConfig {
            device_pairing_enabled: true,
            ..PairingConfig::default()
        },
    ));
    let pair = |name: &str| {
        let request = devices
            .initiate(name, vec![], None, None, None, None)
            .unwrap();
        devices.approve(&request.code, "admin").unwrap().0
    };
    let (phone, theirs) = (pair("alice's phone"), pair("bob's phone"));
    hub.lifecycle_mut().devices = Some(Arc::clone(&devices));
    let alice =
        Identity::new("Alice", IdentityRole::Member).with_linked_id("ios_device", &phone.id);
    let bob = Identity::new("Bob", IdentityRole::Member).with_linked_id("ios_device", &theirs.id);
    hub.identities.create(&alice).unwrap();
    hub.identities.create(&bob).unwrap();
    let admin = hub.admin();

    let report = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    assert_eq!(
        Hub::category(&report, Category::Devices)
            .tally
            .conflicts
            .len(),
        1
    );
    let left: Vec<String> = devices
        .list_devices()
        .unwrap()
        .into_iter()
        .filter(|d| !d.revoked)
        .map(|d| d.id)
        .collect();
    assert_eq!(left, [theirs.id], "the displaced device's token is revoked");
}

#[test]
fn a_linked_id_holding_a_colon_is_never_read_as_a_platform() {
    let teams = Identity::new("Tea", IdentityRole::Member).with_linked_id("teams", "29:1abc");
    let person = Person::of(&teams);
    assert_eq!(person.linked(), [("teams", "29:1abc")]);

    let recorded = person.names.clone();
    let retired = Identity::new("Tea", IdentityRole::Member);
    let resumed = Person {
        id: teams.id,
        ..Person::of(&retired)
    }
    .with_names(&recorded);
    assert_eq!(resumed.linked(), [("teams", "29:1abc")]);
    assert!(resumed.names.contains(&"29:1abc".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_merge_carries_a_colon_bearing_link_whole() {
    let hub = hub().await;
    let alice = Identity::new("Alice", IdentityRole::Member).with_linked_id("teams", "29:1abc");
    hub.identities.create(&alice).unwrap();
    let bob = hub.person("Bob", "U-B");
    let admin = hub.admin();

    assert!(
        hub.lifecycle
            .merge(alice.id, bob.id, &admin, false)
            .await
            .unwrap()
            .complete
    );

    let links = hub
        .identities
        .get_by_id(bob.id)
        .unwrap()
        .unwrap()
        .linked_ids;
    assert_eq!(links.get("teams").map(String::as_str), Some("29:1abc"));
    assert_eq!(links.len(), 2, "no fake platform was invented: {links:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_while_another_operation_runs_moves_nothing() {
    let hub = hub().await;
    let (alice, carol) = (hub.person("Alice", "U-A"), hub.person("Carol", "U-C"));
    let admin = hub.admin();
    let job = hub.job_for(&alice).await;
    let running = workforce_identity::lifecycle::LifecycleOp {
        id: Uuid::new_v4(),
        subject: alice.id,
        kind: "delete".into(),
        target: None,
        actor: "elsewhere".into(),
        names: vec![],
        status: OpStatus::Running,
        report: serde_json::json!({}),
        started_at: Utc::now(),
        lease_until: Utc::now() + Duration::minutes(5),
        finished_at: None,
    };
    assert!(hub.identities.claim_op(&running, Utc::now()).unwrap());
    let item = Item::Job {
        agent: hub.service.agent_id.as_str().to_string(),
        id: job.clone(),
    };

    let refused = hub
        .lifecycle
        .transfer(alice.id, &item, carol.id, &admin)
        .await;

    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );
    assert_eq!(
        hub.scheduler()
            .get_job(&job)
            .await
            .unwrap()
            .creator_identity_id,
        Some(alice.id.to_string())
    );
}

/// A workflow run of `workflow_id` with `variables`.
pub(super) fn run(
    id: &str,
    workflow_id: &str,
    status: &str,
    variables: serde_json::Value,
) -> workforce_common::persistence::WorkflowInstanceRow {
    workforce_common::persistence::WorkflowInstanceRow {
        id: id.into(),
        workflow_id: workflow_id.into(),
        status: status.into(),
        step_history: "[]".into(),
        variables: variables.to_string(),
        trigger_json: "{}".into(),
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

/// Alice's neighbour shares the start of her identity id and of her Slack
/// id, a shared room, a team and a workflow of hers; deleting Alice takes
/// only what is hers.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_takes_nothing_of_a_neighbour_whose_names_start_like_theirs() {
    use workforce_identity::team::Team;
    let hub = hub().await;
    let alice = hub.person("Alice", "U-A");
    let mut near = Identity::new("Near", IdentityRole::Member).with_linked_id("slack", "U-AB");
    let mut id = alice.id.to_string();
    id.replace_range(35.., if id.ends_with('0') { "1" } else { "0" });
    near.id = Uuid::parse_str(&id).unwrap();
    hub.identities.create(&near).unwrap();
    let admin = hub.admin();
    let agent = hub.service.agent_id.as_str().to_string();
    let sessions = &hub.service.sessions;

    let alices_dm = sessions.get_or_create("dm", "U-A", &agent).unwrap();
    let nears_dm = sessions.get_or_create("dm", "U-AB", &agent).unwrap();
    let nears_own = sessions
        .get_or_create("web", &near.id.to_string(), &agent)
        .unwrap();
    let mut room = sessions
        .get_or_create("slack:C1", "shared:1700.1", &agent)
        .unwrap();
    room.add_user_message("alice says hello", None);
    sessions.save(&room).unwrap();
    hub.memory_of(&alice, "alice likes tea").await;
    let nears_memory = hub.memory_of(&near, "near likes coffee").await;
    hub.write_note(&near, "2026-10-01.md", "near's day");
    let alices_job = hub.job_for(&alice).await;
    let nears_job = hub.job_for(&near).await;
    let mut team = Team::new("Ops", near.id);
    team.add_member(alice.id);
    team.add_member(near.id);
    hub.identities.create_team(&team).unwrap();

    let telemetry = Arc::clone(&hub.service.telemetry_store);
    let (a, n) = (alice.id.to_string(), near.id.to_string());
    for row in [
        run(
            "alices",
            &format!("{a}/digest"),
            "running",
            serde_json::json!({"_run_as": a}),
        ),
        run(
            "nears-of-alices",
            &format!("{a}/digest"),
            "running",
            serde_json::json!({"_run_as": n}),
        ),
        run(
            "nears",
            &format!("{n}/digest"),
            "completed",
            serde_json::json!({"_run_as": n}),
        ),
        run(
            "billed",
            &format!("{n}/report"),
            "completed",
            serde_json::json!({"_run_as": n, "_bill_to": a}),
        ),
    ] {
        telemetry.save_workflow_instance(&row).unwrap();
    }

    let report = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();
    assert!(report.complete, "{report:#?}");

    assert!(sessions.get(&alices_dm.id).unwrap().is_none());
    for kept in [&nears_dm.id, &nears_own.id, &room.id] {
        assert!(
            sessions.get(kept).unwrap().is_some(),
            "{kept} is not theirs"
        );
    }
    assert_eq!(
        sessions.get(&room.id).unwrap().unwrap().messages.len(),
        1,
        "a shared room keeps what everyone said in it"
    );
    assert!(hub.memories().get(&nears_memory).await.unwrap().is_some());
    assert!(hub.notes(&near).exists());
    assert!(hub.scheduler().get_job(&alices_job).await.is_none());
    assert!(hub.scheduler().get_job(&nears_job).await.is_some());
    let team = hub.identities.get_team(team.id).unwrap().unwrap();
    assert!(!team.has_member(alice.id) && team.has_member(near.id));
    assert!(telemetry.get_workflow_instance("alices").unwrap().is_none());
    let started_by_near = telemetry
        .get_workflow_instance("nears-of-alices")
        .unwrap()
        .expect("a run acting as someone else is theirs");
    assert_eq!(started_by_near.status, "cancelled", "its workflow is gone");
    assert!(telemetry.get_workflow_instance("nears").unwrap().is_some());
    assert!(
        telemetry.get_workflow_instance("billed").unwrap().is_some(),
        "a run that only bills them is a billing record"
    );
}

#[test]
fn a_vault_delete_takes_only_paths_the_person_is_a_segment_of() {
    let dir = tempfile::tempdir().unwrap();
    let vault = crate::vault::Vault::open(
        &dir.path().join("vault.enc"),
        &crate::vault::Vault::generate_master_key(),
    )
    .unwrap();
    let alice = Identity::new("Alice", IdentityRole::Member).with_linked_id("slack", "U-A");
    let a = alice.id.to_string();
    let others = [
        "users/U-AB/github/token".to_string(),
        format!("users/{a}x/github/token"),
        format!("notes/{a}/github/token"),
        format!("shared/users-{a}/key"),
    ];
    for path in others.iter().chain(
        [
            format!("users/{a}/github/token"),
            "users/U-A/slack/token".to_string(),
        ]
        .iter(),
    ) {
        vault.set(path, "v").unwrap();
    }

    let erased = super::records::delete_in(&vault, "global", &Person::of(&alice), true).unwrap();

    assert_eq!(erased.count, 2);
    let mut left = vault.list(None);
    left.sort();
    let mut expected = others.to_vec();
    expected.sort();
    assert_eq!(left, expected);
}

/// A rule that only Alice could approve keeps refusing once she is merged
/// away: emptied, it would stop routing and fall to self-approval.
#[tokio::test(flavor = "multi_thread")]
async fn a_merge_never_turns_a_gated_tool_into_self_approval() {
    use workforce_permissions::types::{ApproverRef, PermissionRule, PermissionScope};
    let hub = hub().await;
    let (alice, bob, carol) = (
        hub.person("Alice", "U-A"),
        hub.person("Bob", "U-B"),
        hub.person("Carol", "U-C"),
    );
    let admin = hub.admin();
    let store: Arc<dyn workforce_permissions::store::PermissionStore> =
        Arc::new(workforce_permissions::sqlite::SqlitePermissionStore::in_memory().unwrap());
    hub.service.set_permission_evaluator(Arc::new(
        workforce_permissions::cache::PermissionEvaluator::new(Arc::clone(&store)),
    ));
    let gate = |tool: &str, approvers: Vec<ApproverRef>| {
        PermissionRule::new(
            PermissionScope::Agent,
            "main",
            tool,
            workforce_permissions::types::PermissionState::Ask,
            workforce_permissions::types::PermissionOrigin::Admin,
        )
        .with_approvers(approvers)
        .with_allow_self_approval(false)
    };
    let only = gate(
        "vault.delete",
        vec![ApproverRef::User {
            id: alice.id.to_string(),
        }],
    );
    let shared = gate(
        "agent.delete",
        vec![
            ApproverRef::User {
                id: alice.id.to_string(),
            },
            ApproverRef::User {
                id: carol.id.to_string(),
            },
        ],
    );
    store.upsert_rule(&only).unwrap();
    store.upsert_rule(&shared).unwrap();

    let report = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    assert_eq!(
        store.get_rule(only.id).unwrap().unwrap().approvers,
        only.approvers,
        "a rule left with no approver would let the requester approve themselves"
    );
    assert_eq!(
        store.get_rule(shared.id).unwrap().unwrap().approvers,
        [ApproverRef::User {
            id: carol.id.to_string()
        }],
        "authority is not carried to the identity merged into"
    );
    assert_eq!(
        Hub::category(&report, Category::ApproverSlots)
            .tally
            .conflicts
            .len(),
        1
    );
}

/// Every file and directory under `root` with a hash of its content.
/// SQLite files are left out: each store kept in one is dumped by what it
/// holds instead, since a read may touch its journal without changing it.
fn tree(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Option<u64>> {
    use std::hash::{Hash, Hasher};
    let mut found = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.to_string_lossy().to_string();
            if [".db", "-wal", "-shm", "-journal"]
                .iter()
                .any(|s| name.ends_with(s))
            {
                continue;
            }
            if path.is_dir() {
                found.insert(path.clone(), None);
                pending.push(path);
            } else {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::fs::read(&path).unwrap().hash(&mut hasher);
                found.insert(path, Some(hasher.finish()));
            }
        }
    }
    found
}

/// What every store the test hub has holds, as text: compared before and
/// after a dry run.
async fn snapshot(hub: &Hub, people: &[&Identity]) -> String {
    let agent = hub.service.agent_id.clone();
    let mut out = format!("{:?}\n", tree(hub._dir.path()));
    let identities = hub
        .identities
        .list(&workforce_identity::store::IdentityFilter::default())
        .unwrap();
    out += &format!(
        "{identities:?}\n{:?}\n",
        hub.identities.list_teams().unwrap()
    );
    out += &format!("{:?}\n", hub.identities.list_ops(None, 100).unwrap());
    out += &serde_json::to_string(&hub.scheduler().list_jobs().await).unwrap();
    let telemetry = &hub.service.telemetry_store;
    out += &format!(
        "{:?}\n",
        telemetry
            .workflow_instances_after(chrono::DateTime::<Utc>::UNIX_EPOCH, "", 1000)
            .unwrap()
    );
    for who in people {
        let id = who.id.to_string();
        out += &format!(
            "{:?} {:?} {:?}\n",
            hub.memories().identity_memories(&agent, &id).await.unwrap(),
            hub.service.sessions.list_for_owner(&id, 100).unwrap(),
            telemetry.identity_signals(&id).unwrap(),
        );
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dry_run_merge_or_delete_changes_no_store() {
    use workforce_identity::team::Team;
    let hub = hub().await;
    let (alice, bob) = (hub.person("Alice", "U-A"), hub.person("Bob", "U-B"));
    let admin = hub.admin();
    let a = alice.id.to_string();
    hub.job_for(&alice).await;
    hub.memory_of(&alice, "alice likes tea").await;
    hub.write_note(&alice, "2026-10-01.md", "alice's day");
    hub.write_note(&bob, "2026-10-01.md", "bob's day");
    let profile = hub.service.workspace.join("users");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join(format!("{a}.md")), "alice").unwrap();
    let agent = hub.service.agent_id.as_str().to_string();
    for (channel, owner) in [("dm", a.as_str()), ("dm", "U-A")] {
        let mut session = hub
            .service
            .sessions
            .get_or_create(channel, owner, &agent)
            .unwrap();
        session.add_user_message("hi", None);
        hub.service.sessions.save(&session).unwrap();
    }
    hub.service
        .sessions
        .get_or_create("dm", &bob.id.to_string(), &agent)
        .unwrap();
    let mut team = Team::new("Ops", alice.id);
    team.add_member(alice.id);
    hub.identities.create_team(&team).unwrap();
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    telemetry
        .save_workflow_instance(&run(
            "r1",
            &format!("{a}/digest"),
            "running",
            serde_json::json!({"_run_as": a}),
        ))
        .unwrap();
    telemetry
        .record_cost(&workforce_common::persistence::CostRecord {
            agent_id: agent.clone(),
            model: "m".into(),
            cost_usd: 1.0,
            identity_id: Some(a.clone()),
            description: Some("summarise my letters".into()),
            ..Default::default()
        })
        .unwrap();
    let check = workforce_common::heartbeat_check::parse_heartbeat_check(
        "inbox",
        &format!("+++\nname = \"Inbox\"\ncreator = \"{a}\"\n+++\nRead my inbox."),
    )
    .unwrap();
    workforce_common::heartbeat_check::save_heartbeat_check(&hub.service.workspace, &check)
        .unwrap();
    let before = snapshot(&hub, &[&alice, &bob]).await;

    let merge = hub
        .lifecycle
        .merge(alice.id, bob.id, &admin, true)
        .await
        .unwrap();
    let delete = hub
        .lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, true)
        .await
        .unwrap();

    for report in [&merge, &delete] {
        assert!(report.dry_run && !report.complete);
        let held: usize = report.categories.iter().map(|c| c.tally.count).sum();
        assert!(held >= 8, "the dry run saw what is there: {report:#?}");
    }
    assert_eq!(snapshot(&hub, &[&alice, &bob]).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_moves_only_a_job_the_subject_owns() {
    let hub = hub().await;
    let (alice, carol, dave) = (
        hub.person("Alice", "U-A"),
        hub.person("Carol", "U-C"),
        hub.person("Dave", "U-D"),
    );
    let admin = hub.admin();
    let carols = hub.job_for(&carol).await;
    hub.scheduler()
        .update_job_bill_to(&carols, Some(alice.id.to_string()))
        .await;
    let item = Item::Job {
        agent: hub.service.agent_id.as_str().to_string(),
        id: carols.clone(),
    };

    let refused = hub
        .lifecycle
        .transfer(alice.id, &item, dave.id, &admin)
        .await;

    assert!(
        matches!(refused, Err(LifecycleError::NotFound(_))),
        "{refused:?}"
    );
    let job = hub.scheduler().get_job(&carols).await.unwrap();
    assert_eq!(job.creator_identity_id, Some(carol.id.to_string()));
    assert_eq!(job.bill_to_identity_id, Some(alice.id.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_persons_check_is_paused_and_a_merged_ones_keeps_running() {
    let hub = hub().await;
    let (alice, bob, carol) = (
        hub.person("Alice", "U-A"),
        hub.person("Bob", "U-B"),
        hub.person("Carol", "U-C"),
    );
    let admin = hub.admin();
    let workspace = &hub.service.workspace;
    for (id, who) in [("alices", &alice), ("carols", &carol)] {
        let check = workforce_common::heartbeat_check::parse_heartbeat_check(
            id,
            &format!(
                "+++\nname = \"{id}\"\ncreator = \"{}\"\n+++\nRead my inbox.",
                who.id
            ),
        )
        .unwrap();
        workforce_common::heartbeat_check::save_heartbeat_check(workspace, &check).unwrap();
    }
    let check = |id: &str| {
        workforce_common::heartbeat_check::list_heartbeat_checks(workspace)
            .unwrap()
            .into_iter()
            .find(|c| c.id == id)
            .unwrap()
    };

    assert!(
        hub.lifecycle
            .merge(carol.id, bob.id, &admin, false)
            .await
            .unwrap()
            .complete
    );
    assert!(
        hub.lifecycle
            .delete(alice.id, &DeletePlan::default(), &admin, false)
            .await
            .unwrap()
            .complete
    );

    let released = check("alices");
    assert_eq!(released.meta.creator, None);
    assert!(
        !released.meta.enabled,
        "it must not start running as the agent"
    );
    let merged = check("carols");
    assert_eq!(merged.meta.creator, Some(bob.id.to_string()));
    assert!(merged.meta.enabled);
}
