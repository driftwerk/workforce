use std::sync::Arc;

use chrono::Utc;
use workforce_identity::lifecycle::{LifecycleOp, LifecycleOpStore, OpStatus};
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityStatus};

use super::tests::{hub, run, Hub};
use super::*;
use crate::hub_core::schedules::JobEdit;

/// A hub whose agent reads the same identity store the lifecycle does, as
/// the installed hub's agents do.
async fn standing_hub() -> Hub {
    let hub = hub().await;
    hub.service
        .set_identity_store(Arc::clone(&hub.identities) as Arc<dyn IdentityStore>);
    hub
}

/// One job the person keeps running and one they switched off themselves.
async fn running_and_paused(hub: &Hub, who: &Identity) -> (String, String) {
    let running = hub.job_for(who).await;
    let paused = hub.job_for(who).await;
    hub.scheduler()
        .update_job(
            &paused,
            JobEdit {
                enabled: Some(false),
                ..JobEdit::default()
            },
            &who.id.to_string(),
        )
        .await
        .unwrap();
    (running, paused)
}

async fn job(hub: &Hub, id: &str) -> crate::hub_core::schedules::ScheduledJob {
    hub.scheduler().get_job(id).await.expect("the job")
}

fn status(hub: &Hub, who: &Identity) -> IdentityStatus {
    hub.identities.get_by_id(who.id).unwrap().unwrap().status
}

fn kinds(hub: &Hub, who: &Identity) -> Vec<(String, OpStatus)> {
    hub.identities
        .list_ops(Some(who.id), 10)
        .unwrap()
        .into_iter()
        .map(|op| (op.kind, op.status))
        .collect()
}

#[test]
fn a_suspension_changes_exactly_the_categories_whose_rule_says_it_does() {
    for category in Category::ALL {
        let changes = matches!(
            category.rules().suspend,
            SuspendRule::Hold | SuspendRule::Stop
        );
        assert_eq!(pausable(category).is_some(), changes, "{category:?}");
    }
}

#[tokio::test]
async fn suspending_holds_what_runs_as_them_and_resuming_releases_exactly_that() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    let (running, paused) = running_and_paused(&hub, &ada).await;

    let report = hub.lifecycle.suspend(ada.id, &admin).await.unwrap();
    assert!(report.complete, "{report:?}");
    assert_eq!(status(&hub, &ada), IdentityStatus::Suspended);
    assert_eq!(
        Hub::category(&report, Category::ScheduledJobs).tally.count,
        1
    );
    let held = job(&hub, &running).await;
    assert!(!held.enabled && held.paused_by_suspension);
    assert!(!job(&hub, &paused).await.paused_by_suspension);

    let report = hub.lifecycle.resume(ada.id, &admin).await.unwrap();
    assert!(report.complete, "{report:?}");
    assert_eq!(status(&hub, &ada), IdentityStatus::Active);
    let released = job(&hub, &running).await;
    assert!(released.enabled && !released.paused_by_suspension);
    assert!(
        !job(&hub, &paused).await.enabled,
        "what they paused themselves stays paused"
    );
    assert_eq!(
        kinds(&hub, &ada),
        [
            ("resume".to_string(), OpStatus::Complete),
            ("suspend".to_string(), OpStatus::Complete)
        ]
    );
}

#[tokio::test]
async fn following_a_standing_twice_changes_nothing_more() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    hub.job_for(&ada).await;
    hub.lifecycle.suspend(ada.id, &admin).await.unwrap();

    let again = hub.lifecycle.follow_standing(ada.id, &admin).await.unwrap();
    assert_eq!(again.operation, "suspend");
    assert_eq!(
        Hub::category(&again, Category::ScheduledJobs).tally.count,
        0
    );
    hub.lifecycle.resume(ada.id, &admin).await.unwrap();
    let again = hub.lifecycle.follow_standing(ada.id, &admin).await.unwrap();
    assert_eq!(again.operation, "resume");
    assert_eq!(
        Hub::category(&again, Category::ScheduledJobs).tally.count,
        0
    );
}

/// A status written straight to the store (an older replica's API, a
/// merge, a database edit) is followed as soon as anything asks.
#[tokio::test]
async fn a_status_set_by_any_path_is_followed() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    let id = hub.job_for(&ada).await;
    let set = |status| {
        let mut identity = hub.identities.get_by_id(ada.id).unwrap().unwrap();
        identity.status = status;
        hub.identities.update(&identity).unwrap();
    };

    set(IdentityStatus::Suspended);
    hub.lifecycle.follow_standing(ada.id, &admin).await.unwrap();
    assert!(job(&hub, &id).await.paused_by_suspension);

    set(IdentityStatus::Active);
    let released = hub
        .scheduler()
        .release_suspension_holds(&|person| hub.service.suspended(person).is_some(), true)
        .await;
    assert_eq!(
        released.len(),
        1,
        "the scheduler's own review releases it without the lifecycle"
    );
    assert!(job(&hub, &id).await.enabled);
}

#[tokio::test]
async fn an_admin_cannot_suspend_themselves_and_only_a_suspended_person_resumes() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    let own = hub.lifecycle.suspend(admin.0.unwrap(), &admin).await;
    assert!(matches!(own, Err(LifecycleError::Refused(_))), "{own:?}");
    let active = hub.lifecycle.resume(ada.id, &admin).await;
    assert!(
        matches!(active, Err(LifecycleError::Refused(_))),
        "{active:?}"
    );
    assert_eq!(status(&hub, &ada), IdentityStatus::Active);
}

#[tokio::test]
async fn a_live_lease_on_the_person_refuses_a_suspension_sweep() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    let id = hub.job_for(&ada).await;
    let held = LifecycleOp {
        id: Uuid::new_v4(),
        subject: ada.id,
        kind: "merge".into(),
        target: None,
        actor: "elsewhere".into(),
        names: vec![],
        status: OpStatus::Running,
        report: serde_json::json!({}),
        started_at: Utc::now(),
        lease_until: Utc::now() + Duration::minutes(5),
        finished_at: None,
    };
    assert!(hub.identities.claim_op(&held, Utc::now()).unwrap());

    let refused = hub.lifecycle.suspend(ada.id, &admin).await;
    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );
    assert_eq!(
        status(&hub, &ada),
        IdentityStatus::Suspended,
        "the status stands; the live checks refuse them meanwhile"
    );
    assert!(job(&hub, &id).await.enabled);
}

#[tokio::test]
async fn a_held_job_merged_into_someone_active_runs_again_as_them() {
    let hub = standing_hub().await;
    let (ada, bo) = (hub.person("Ada", "U-A"), hub.person("Bo", "U-B"));
    let admin = hub.admin();
    let id = hub.job_for(&ada).await;
    hub.lifecycle.suspend(ada.id, &admin).await.unwrap();

    let report = hub
        .lifecycle
        .merge(ada.id, bo.id, &admin, false)
        .await
        .unwrap();
    assert!(report.complete, "{report:?}");
    let merged = job(&hub, &id).await;
    assert_eq!(merged.creator_identity_id, Some(bo.id.to_string()));
    assert!(merged.enabled && !merged.paused_by_suspension);
}

#[tokio::test]
async fn a_suspension_lists_the_runs_it_holds_and_a_resume_the_runs_it_releases() {
    let hub = standing_hub().await;
    let ada = hub.person("Ada", "U-A");
    let admin = hub.admin();
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    let person = ada.id.to_string();
    for (id, status, held) in [
        ("open", "running", false),
        ("held", "paused", true),
        ("theirs-paused", "paused", false),
        ("done", "completed", false),
    ] {
        let mut variables = serde_json::json!({"_run_as": person});
        if held {
            variables["_suspension_held_at"] = serde_json::json!(Utc::now().to_rfc3339());
        }
        telemetry
            .save_workflow_instance(&run(id, "flow", status, variables))
            .unwrap();
    }

    let held = hub.lifecycle.suspend(ada.id, &admin).await.unwrap();
    let runs = Hub::category(&held, Category::WorkflowRuns);
    let mut items = runs.tally.items.clone();
    items.sort();
    assert_eq!(
        items,
        ["flow (paused)", "flow (paused)", "flow (running)"],
        "every open run, finished ones left out"
    );

    let released = hub.lifecycle.resume(ada.id, &admin).await.unwrap();
    let runs = Hub::category(&released, Category::WorkflowRuns);
    assert_eq!(runs.tally.items, ["flow (paused)"]);
}
