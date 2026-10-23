use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::Utc;
use workforce_identity::lifecycle::{LifecycleOp, LifecycleOpStore, OpStatus};
use workforce_identity::remote_mcp_token::{RemoteMcpToken, RemoteMcpTokenStore};
use workforce_identity::store::IdentityStore;
use workforce_identity::team::{Team, TeamStore};
use workforce_identity::types::Identity;

use super::orphans::{Class, Directory};
use super::tests::{hub, run, Hub};
use super::*;

/// Someone whose identity was deleted by hand, leaving no record of it.
fn gone(hub: &Hub, name: &str, slack: &str) -> Identity {
    let who = hub.person(name, slack);
    hub.identities.delete(who.id).unwrap();
    who
}

fn session(hub: &Hub, channel: &str, owner: &str) -> workforce_common::session::Session {
    let mut session = hub
        .service
        .sessions
        .get_or_create(channel, owner, hub.service.agent_id.as_str())
        .unwrap();
    session.add_user_message("hello", None);
    hub.service.sessions.save(&session).unwrap();
    session
}

fn spend(hub: &Hub, identity: &str) {
    hub.service
        .telemetry_store
        .record_cost(&workforce_common::persistence::CostRecord {
            agent_id: hub.service.agent_id.as_str().to_string(),
            model: "m".into(),
            cost_usd: 1.0,
            identity_id: Some(identity.to_string()),
            ..Default::default()
        })
        .unwrap();
}

fn check_by(hub: &Hub, id: &str, creator: &str) {
    let check = workforce_common::heartbeat_check::parse_heartbeat_check(
        id,
        &format!("+++\nname = \"{id}\"\ncreator = \"{creator}\"\n+++\nRead my inbox."),
    )
    .unwrap();
    workforce_common::heartbeat_check::save_heartbeat_check(&hub.service.workspace, &check)
        .unwrap();
}

/// A finished lifecycle operation on `subject` that recorded `names`.
fn recorded(subject: Uuid, names: &[&str]) -> LifecycleOp {
    LifecycleOp {
        id: Uuid::new_v4(),
        subject,
        kind: "delete".into(),
        target: None,
        actor: "admin".into(),
        names: std::iter::once(subject.to_string())
            .chain(names.iter().map(|n| n.to_string()))
            .collect(),
        status: OpStatus::Complete,
        report: serde_json::json!({}),
        started_at: Utc::now(),
        lease_until: Utc::now(),
        finished_at: Some(Utc::now()),
    }
}

fn found(scan: &OrphanScan, id: Uuid) -> &Orphan {
    scan.orphans
        .iter()
        .find(|orphan| orphan.id == id)
        .unwrap_or_else(|| panic!("{id} is not listed: {scan:#?}"))
}

/// The case behind the clean-up: an identity deleted by hand, whose job is
/// already gone, left conversations and memories under its id. Its billing
/// proves it was an identity; nothing held under a room, the hub, an edge
/// customer, an unlinked platform id or an id nothing vouches for is taken
/// for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_hand_deleted_identity_is_found_through_its_billing_and_nothing_else_is_taken() {
    let hub = hub().await;
    let admin = hub.admin();
    let bob = hub.person("Bob", "U-B");
    let lost = gone(&hub, "Lost", "U-L");
    let id = lost.id.to_string();
    for channel in ["dm", "web", "slack:C1"] {
        session(&hub, channel, &id);
    }
    hub.memory_of(&lost, "likes tea").await;
    spend(&hub, &id);
    let stray = Uuid::new_v4().to_string();
    for owner in [
        bob.id.to_string().as_str(),
        "shared:1700.1",
        "system:digest",
        "ext:issuer\u{241f}sub",
        "U-NEVER",
        stray.as_str(),
    ] {
        session(&hub, "dm", owner);
    }

    let scan = hub.lifecycle.orphans(&admin).await.unwrap();

    for category in [Category::Sessions, Category::Memories, Category::CostLedger] {
        assert!(!scan.failed.contains_key(&category), "{scan:#?}");
    }
    assert_eq!(scan.orphans.len(), 1, "{scan:#?}");
    let orphan = found(&scan, lost.id);
    assert_eq!(orphan.proof, Proof::IdentityField);
    assert_eq!(
        orphan.held,
        BTreeMap::from([(Category::Sessions, 3), (Category::Memories, 1)]),
        "billing is kept whatever happens, so it is not left over"
    );
    assert!(
        scan.unproven.contains(&Named {
            name: stray,
            could_be: vec![],
            held: BTreeMap::from([(Category::Sessions, 1)]),
        }),
        "{scan:#?}"
    );
    for kind in [
        OwnerKind::Live,
        OwnerKind::Shared,
        OwnerKind::System,
        OwnerKind::External,
        OwnerKind::Other,
    ] {
        assert!(scan.not_orphaned.contains_key(&kind), "{kind:?}: {scan:#?}");
    }
}

#[test]
fn only_a_name_that_provably_named_someone_now_gone_is_an_orphan() {
    let (deleted, proven, twice_a, twice_b, merged, team) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (live, device) = (Uuid::new_v4(), Uuid::new_v4().to_string());
    let into_live = LifecycleOp {
        kind: "merge".into(),
        target: Some(live),
        ..recorded(merged, &["slack:U-MERGED", "U-MERGED"])
    };
    let directory = Directory::new(
        super::orphans::Known {
            live: [live.to_string(), "slack:U-RELINK".into(), "U-RELINK".into()]
                .iter()
                .map(|name| fold(name))
                .collect(),
            devices: BTreeSet::from([device.clone()]),
            teams: BTreeSet::from([team]),
            ops: vec![
                recorded(
                    deleted,
                    &["slack:U-DEL", "U-DEL", "slack:U-RELINK", "U-RELINK"],
                ),
                recorded(twice_a, &["slack:U-TWICE", "U-TWICE"]),
                recorded(twice_b, &["slack:U-TWICE", "U-TWICE"]),
                into_live,
            ],
            history_read: true,
        },
        &BTreeSet::from([
            proven.to_string(),
            device.clone(),
            live.to_string(),
            team.to_string(),
        ]),
    );

    for (name, class) in [
        (deleted.to_string(), Class::Orphan(deleted)),
        ("U-DEL".into(), Class::Orphan(deleted)),
        ("slack:U-DEL".into(), Class::Orphan(deleted)),
        (proven.to_string(), Class::Orphan(proven)),
        (merged.to_string(), Class::Orphan(merged)),
        ("U-MERGED".into(), Class::Not(OwnerKind::Live)),
        ("slack:U-MERGED".into(), Class::Not(OwnerKind::Live)),
        ("U-RELINK".into(), Class::Not(OwnerKind::Live)),
        ("u-relink".into(), Class::Not(OwnerKind::Live)),
        (live.to_string(), Class::Not(OwnerKind::Live)),
        (live.to_string().to_uppercase(), Class::Not(OwnerKind::Live)),
        (format!("{{{live}}}"), Class::Not(OwnerKind::Live)),
        (format!("urn:uuid:{live}"), Class::Not(OwnerKind::Live)),
        (live.simple().to_string(), Class::Not(OwnerKind::Live)),
        (team.to_string(), Class::Not(OwnerKind::Team)),
        (
            "U-TWICE".into(),
            Class::Ambiguous(vec![twice_a.min(twice_b), twice_a.max(twice_b)]),
        ),
        (device.clone(), Class::Not(OwnerKind::Device)),
        (device.to_uppercase(), Class::Not(OwnerKind::Device)),
        (Uuid::new_v4().to_string(), Class::Unproven),
        (proven.to_string().to_uppercase(), Class::Unproven),
        (format!("{{{proven}}}"), Class::Unproven),
        (deleted.to_string().to_uppercase(), Class::Unproven),
        ("shared:1700.1".into(), Class::Not(OwnerKind::Shared)),
        ("system".into(), Class::Not(OwnerKind::System)),
        ("system:digest".into(), Class::Not(OwnerKind::System)),
        ("scheduler".into(), Class::Not(OwnerKind::System)),
        ("rest:owner-token".into(), Class::Not(OwnerKind::System)),
        ("agent:main".into(), Class::Not(OwnerKind::Agent)),
        (
            "ext:issuer\u{241f}sub".into(),
            Class::Not(OwnerKind::External),
        ),
        ("systematic".into(), Class::Not(OwnerKind::Other)),
        ("U-NEVER".into(), Class::Not(OwnerKind::Other)),
    ] {
        assert_eq!(directory.classify(&name), class, "{name}");
    }
}

/// Every category says whom it holds things for, and says it as its own
/// `held` counts it for that person, so the scan and the clean-up agree.
#[tokio::test(flavor = "multi_thread")]
async fn every_category_answers_whom_it_holds_things_for() {
    use workforce_orchestrator::artifact_store::{
        Artifact, ArtifactBackend, ArtifactType, FsArtifactBackend, Visibility,
    };
    use workforce_pairing::{
        ChannelAllowlistEntry, ChannelPairingService, DevicePairingService, PairingConfig,
        PairingStore, SqlitePairingStore,
    };
    use workforce_permissions::types::{
        ApproverRef, PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef, PermissionRule,
        PermissionScope, PermissionState,
    };
    let mut hub = hub().await;
    let pairing = Arc::new(SqlitePairingStore::in_memory().unwrap());
    let devices = Arc::new(DevicePairingService::new(
        Arc::clone(&pairing) as Arc<dyn PairingStore>,
        PairingConfig {
            device_pairing_enabled: true,
            ..PairingConfig::default()
        },
    ));
    let request = devices
        .initiate("phone", vec![], None, None, None, None)
        .unwrap();
    let phone = devices.approve(&request.code, "admin").unwrap().0;
    pairing
        .add_to_allowlist(&ChannelAllowlistEntry {
            id: "e1".into(),
            platform: "slack".into(),
            channel_id: "C1".into(),
            sender_id: "U-A".into(),
            sender_name: "Alice".into(),
            added_at: Utc::now(),
            added_by: "manual".into(),
        })
        .unwrap();
    hub.lifecycle_mut().devices = Some(devices);
    hub.lifecycle_mut().channels = Some(Arc::new(ChannelPairingService::new(
        Arc::clone(&pairing) as Arc<dyn PairingStore>,
        PairingConfig::default(),
    )));
    let (alice, carol) = (hub.person("Alice", "U-A"), hub.person("Carol", "U-C"));
    let admin = hub.admin();
    let a = alice.id.to_string();

    hub.job_for(&alice).await;
    hub.service
        .telemetry_store
        .save_workflow_instance(&run(
            "r1",
            &format!("{a}/digest"),
            "running",
            serde_json::json!({"_run_as": a}),
        ))
        .unwrap();
    check_by(&hub, "inbox", &a);
    let plan = workforce_common::learning_plan::parse_learning_plan(
        "research",
        &format!("+++\nname = \"Research\"\nrun_as_identity = \"{a}\"\n+++\nRead up."),
    )
    .unwrap();
    workforce_common::learning_plan::write_learning_plan(&hub.service.workspace, &plan).unwrap();
    session(&hub, "dm", &a);
    session(&hub, "web", &a);
    hub.memory_of(&alice, "likes tea").await;
    hub.memory_of(&alice, "likes cake").await;
    hub.write_note(&alice, "2026-10-01.md", "alice's day");
    let profiles = hub.service.workspace.join("users");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::write(profiles.join(format!("{a}.md")), "alice").unwrap();
    workforce_games::RivalryStore::record(
        crate::games_driver::rivalry_store(&hub.lifecycle.data_dir).as_ref(),
        &[workforce_games::PlayRecord {
            human_id: a.clone(),
            agent_id: "bot".into(),
            caught: true,
        }],
    );
    hub.service
        .telemetry_store
        .record_cost(&workforce_common::persistence::CostRecord {
            agent_id: "a".into(),
            model: "m".into(),
            cost_usd: 1.0,
            identity_id: Some(a.clone()),
            description: Some("summarise my letters".into()),
            ..Default::default()
        })
        .unwrap();
    let mut team = Team::new("Ops", carol.id);
    team.remove_member(carol.id);
    team.add_member(alice.id);
    hub.identities.create_team(&team).unwrap();
    hub.identities
        .create_token(&RemoteMcpToken {
            id: Uuid::new_v4(),
            identity_id: alice.id,
            label: "laptop".into(),
            token_hash: "h".into(),
            token_prefix: "p".into(),
            scopes: vec![],
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(1),
            last_used_at: None,
            revoked_at: None,
        })
        .unwrap();
    let permissions: Arc<dyn workforce_permissions::store::PermissionStore> =
        Arc::new(workforce_permissions::sqlite::SqlitePermissionStore::in_memory().unwrap());
    hub.service.set_permission_evaluator(Arc::new(
        workforce_permissions::cache::PermissionEvaluator::new(Arc::clone(&permissions)),
    ));
    let rule = |scope, scope_id: &str, tool: &str| {
        PermissionRule::new(
            scope,
            scope_id,
            tool,
            PermissionState::Ask,
            PermissionOrigin::Admin,
        )
    };
    permissions
        .upsert_rule(&rule(PermissionScope::User, &a, "vault.*"))
        .unwrap();
    permissions
        .upsert_rule(
            &rule(PermissionScope::Agent, "main", "agent.delete").with_approvers(vec![
                ApproverRef::User { id: a.clone() },
                ApproverRef::User {
                    id: carol.id.to_string(),
                },
            ]),
        )
        .unwrap();
    let role = PermissionRoleDef::new(team.id, "operator");
    permissions.upsert_role(&role).unwrap();
    permissions
        .assign_role_to_identity(&PermissionRoleAssignment {
            identity_id: alice.id,
            role_id: role.id,
            granted_by: None,
            granted_at: Utc::now(),
        })
        .unwrap();
    let artifacts = FsArtifactBackend::create_at(&hub.service.workspace).unwrap();
    artifacts
        .create(&Artifact::new(
            "notes",
            ArtifactType::Snippet,
            "notes",
            "content",
            a.clone(),
            a.clone(),
            Visibility::Private,
            vec![],
        ))
        .await
        .unwrap();

    let cx = hub.lifecycle.context(&admin);
    let who = Person::of(&alice);
    let mut answers = BTreeMap::new();
    for category in Category::ALL {
        if let Ok(owners) = holding(category).owners(&cx).await {
            answers.insert(category, owners);
        }
    }
    let of = |category: Category, name: &str| answers[&category].held.get(name).copied();

    for (category, count) in [
        (Category::ScheduledJobs, 1),
        (Category::WorkflowRuns, 1),
        (Category::HeartbeatChecks, 1),
        (Category::LearningPlans, 1),
        (Category::Sessions, 2),
        (Category::Memories, 2),
        (Category::PersonalFiles, 2),
        (Category::GameTallies, 1),
        (Category::Signals, 1),
        (Category::Teams, 1),
        (Category::RemoteMcpTokens, 1),
        (Category::PermissionRules, 1),
        (Category::ApproverSlots, 1),
        (Category::RoleGrants, 1),
        (Category::Artifacts, 1),
    ] {
        assert_eq!(of(category, &a), Some(count), "{category:?}");
        assert_eq!(
            holding(category).held(&cx, &who).await.unwrap().count,
            count,
            "{category:?}: the scan counts what the clean-up acts on"
        );
    }
    assert_eq!(of(Category::Devices, &phone.id), Some(1));
    assert_eq!(of(Category::ChannelAllowlist, "slack:U-A"), Some(1));
    assert_eq!(
        of(Category::Teams, &carol.id.to_string()),
        None,
        "who created a team is a record kept through a delete"
    );
    assert!(answers[&Category::Teams]
        .identities
        .contains(&carol.id.to_string()));
    for category in [
        Category::ScheduledJobs,
        Category::Teams,
        Category::RemoteMcpTokens,
        Category::RoleGrants,
        Category::CostLedger,
    ] {
        assert!(
            answers[&category].identities.contains(&a),
            "{category:?} only ever holds identity ids"
        );
    }
    assert!(
        answers[&Category::CostLedger].held.is_empty(),
        "billing is kept, so there is nothing to act on"
    );
    assert!(answers[&Category::KnowHow].probed);
    for category in [
        Category::IdentityRecord,
        Category::AuditRecords,
        Category::RetentionBound,
    ] {
        assert_eq!(answers[&category], Owners::default(), "{category:?}");
    }
}

#[test]
fn vault_paths_and_budget_overrides_name_whom_they_hold_for() {
    for (path, owners) in [
        ("users/u1/github/token", vec!["u1"]),
        ("wasm-skills/notes/users/u1/key", vec!["u1"]),
        ("wasm-skills/notes/grants/users/u1/_tools", vec!["u1"]),
        ("subscription/device_login/u1", vec!["u1"]),
        ("wasm-skills/users/users/u1/key", vec!["u1", "users"]),
        ("shared/key", vec![]),
    ] {
        assert_eq!(super::records::path_owners(path), owners, "{path}");
    }

    use crate::budget_overrides::{BudgetOverride, OverrideScope};
    let raised = |scope, key: &str| BudgetOverride {
        scope,
        key: key.into(),
        daily_usd: 10.0,
        expires_at: None,
        note: None,
        created_by: "admin".into(),
        updated_at: Utc::now(),
    };
    assert_eq!(
        super::access::identity_overrides(vec![
            raised(OverrideScope::Identity, "u1"),
            raised(OverrideScope::Agent, "main"),
        ]),
        Owners::of(["u1".to_string()])
    );
}

/// The orphan was perhaps someone else: what acts for it goes as the
/// administrator's edit, nothing that grants or signs in reaches the
/// recipient, and what it said moves only on a second, explicit ask.
#[tokio::test(flavor = "multi_thread")]
async fn a_hand_over_gives_work_as_the_admins_edit_and_moves_personal_content_only_when_asked() {
    use workforce_permissions::types::{
        PermissionOrigin, PermissionRoleAssignment, PermissionRoleDef, PermissionRule,
        PermissionScope, PermissionState,
    };
    let hub = hub().await;
    let admin = hub.admin();
    let bob = hub.person("Bob", "U-B");
    let lost = hub.person("Lost", "U-L");
    let job = hub.job_for(&lost).await;
    let memory = hub.memory_of(&lost, "likes tea").await;
    let id = lost.id.to_string();
    let conversation = session(&hub, "dm", &id);
    check_by(&hub, "inbox", &id);
    let mut team = Team::new("Ops", bob.id);
    team.add_member(lost.id);
    hub.identities.create_team(&team).unwrap();
    let permissions: Arc<dyn workforce_permissions::store::PermissionStore> =
        Arc::new(workforce_permissions::sqlite::SqlitePermissionStore::in_memory().unwrap());
    hub.service.set_permission_evaluator(Arc::new(
        workforce_permissions::cache::PermissionEvaluator::new(Arc::clone(&permissions)),
    ));
    let role = PermissionRoleDef::new(team.id, "operator");
    permissions.upsert_role(&role).unwrap();
    permissions
        .assign_role_to_identity(&PermissionRoleAssignment {
            identity_id: lost.id,
            role_id: role.id,
            granted_by: None,
            granted_at: Utc::now(),
        })
        .unwrap();
    permissions
        .upsert_rule(&PermissionRule::new(
            PermissionScope::User,
            &id,
            "vault.*",
            PermissionState::Allow,
            PermissionOrigin::Admin,
        ))
        .unwrap();
    hub.identities.delete(lost.id).unwrap();

    let preview = hub
        .lifecycle
        .transfer_orphan(lost.id, bob.id, false, &admin, true)
        .await
        .unwrap();
    assert!(preview.dry_run && !preview.complete);
    assert_eq!(
        Hub::category(&preview, Category::Sessions).orphan,
        Some(OrphanAction::Keep)
    );
    assert_eq!(
        Hub::category(&preview, Category::ScheduledJobs).orphan,
        Some(OrphanAction::HandOver)
    );
    assert_eq!(
        Hub::category(&preview, Category::RoleGrants).orphan,
        Some(OrphanAction::Erase)
    );
    assert_eq!(
        hub.scheduler()
            .get_job(&job)
            .await
            .unwrap()
            .creator_identity_id,
        Some(id.clone()),
        "a dry run writes nothing"
    );

    let report = hub
        .lifecycle
        .transfer_orphan(lost.id, bob.id, false, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    let moved = hub.scheduler().get_job(&job).await.unwrap();
    assert_eq!(moved.creator_identity_id, Some(bob.id.to_string()));
    assert_eq!(moved.edited_by, Some(admin.name()));
    assert!(
        !crate::scheduler::is_own(&moved),
        "it runs without the recipient's private context until they save it"
    );
    let check = workforce_common::heartbeat_check::list_heartbeat_checks(&hub.service.workspace)
        .unwrap()
        .remove(0);
    assert_eq!(check.meta.creator, None);
    assert!(!check.meta.enabled, "released and paused, never run as bob");
    let team = hub.identities.get_team(team.id).unwrap().unwrap();
    assert_eq!(
        team.members,
        [bob.id],
        "membership is access, never carried"
    );
    assert!(
        permissions.roles_for_identity(bob.id).unwrap().is_empty()
            && permissions.roles_for_identity(lost.id).unwrap().is_empty(),
        "a role is authority: erased, never carried"
    );
    assert!(permissions
        .list_rules(&workforce_permissions::types::PermissionFilter::default())
        .unwrap()
        .iter()
        .all(|rule| rule.scope_id != id && rule.scope_id != bob.id.to_string()));
    assert_eq!(
        hub.identities
            .get_by_id(bob.id)
            .unwrap()
            .unwrap()
            .linked_ids
            .len(),
        1,
        "no name the orphan went by is linked to the recipient"
    );
    assert_eq!(
        hub.service
            .sessions
            .get(&conversation.id)
            .unwrap()
            .unwrap()
            .owner,
        id,
        "nobody confirmed bob was them, so their conversations stay"
    );
    assert!(!hub
        .memories()
        .identity_memories(&hub.service.agent_id, &bob.id.to_string())
        .await
        .unwrap()
        .contains(&memory));
    let left = hub.lifecycle.orphans(&admin).await.unwrap();
    assert_eq!(
        found(&left, lost.id)
            .held
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [Category::Sessions, Category::Memories],
        "only what stayed is left to clean up"
    );

    let personal = hub
        .lifecycle
        .transfer_orphan(lost.id, bob.id, true, &admin, false)
        .await
        .unwrap();

    assert!(personal.complete, "{personal:#?}");
    assert_eq!(
        Hub::category(&personal, Category::Sessions).orphan,
        Some(OrphanAction::Move)
    );
    assert_eq!(
        hub.service
            .sessions
            .get(&conversation.id)
            .unwrap()
            .unwrap()
            .owner,
        bob.id.to_string()
    );
    assert!(hub
        .memories()
        .identity_memories(&hub.service.agent_id, &bob.id.to_string())
        .await
        .unwrap()
        .contains(&memory));
    let ops = hub.lifecycle.operations(Some(lost.id), 10).unwrap();
    assert_eq!(
        (ops[0].kind.as_str(), ops[0].status, ops[0].target),
        ("orphan_transfer", OpStatus::Complete, Some(bob.id))
    );
    assert!(hub
        .lifecycle
        .orphans(&admin)
        .await
        .unwrap()
        .orphans
        .is_empty());
}

/// The hand-over the clean-up was run for: a missing identity's signals and
/// billing, moved with its personal content, leave nothing for the next scan.
#[tokio::test(flavor = "multi_thread")]
async fn a_personal_hand_over_moves_every_signal_the_scan_counted() {
    let hub = hub().await;
    let admin = hub.admin();
    let bob = hub.person("Bob", "U-B");
    let lost = gone(&hub, "Lost", "U-L");
    hub.signals_for(&lost.id.to_string());
    let scan = hub.lifecycle.orphans(&admin).await.unwrap();
    let counted = found(&scan, lost.id).held[&Category::Signals];
    assert_eq!(counted, 3);

    let report = hub
        .lifecycle
        .transfer_orphan(lost.id, bob.id, true, &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    assert_eq!(
        Hub::category(&report, Category::Signals).tally.count,
        counted
    );
    let left = hub.lifecycle.orphans(&admin).await.unwrap();
    assert!(left.orphans.is_empty(), "{left:#?}");
    let telemetry = &hub.service.telemetry_store;
    assert_eq!(
        telemetry
            .identity_signals(&bob.id.to_string())
            .unwrap()
            .total(),
        counted
    );
    assert_eq!(
        telemetry
            .cost_by_identity(chrono::DateTime::<Utc>::UNIX_EPOCH, Utc::now())
            .unwrap(),
        [(bob.id.to_string(), 1.0)]
    );
}

/// What a hand-over does with each category: personal content stays unless
/// asked for, credentials and authority are erased, never carried.
#[test]
fn a_hand_over_moves_personal_content_only_when_asked_and_never_carries_access() {
    for category in Category::ALL {
        let (kept, moved) = (
            OrphanAction::of(category, false),
            OrphanAction::of(category, true),
        );
        match category.rules().orphan {
            OrphanRule::Personal => {
                assert_eq!((kept, moved), (OrphanAction::Keep, OrphanAction::Move))
            }
            OrphanRule::HandOver => assert_eq!(kept, moved, "{category:?}"),
        }
    }
    for category in [
        Category::Vault,
        Category::RoleGrants,
        Category::PermissionRules,
        Category::ApproverSlots,
        Category::BudgetOverrides,
        Category::RemoteMcpTokens,
        Category::Devices,
        Category::ChannelAllowlist,
        Category::Teams,
    ] {
        assert_eq!(
            OrphanAction::of(category, true),
            OrphanAction::Erase,
            "{category:?}"
        );
    }
    for category in [
        Category::ScheduledJobs,
        Category::Definitions,
        Category::Artifacts,
        Category::Skills,
    ] {
        assert_eq!(
            OrphanAction::of(category, false),
            OrphanAction::HandOver,
            "{category:?}"
        );
    }
}

/// A delete finished while an agent was not serving, so its sessions under
/// the person's id and the bare Slack id they first wrote from were left;
/// the names its operation recorded find them.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_deleted_identity_left_under_its_old_names_is_erased_and_its_billing_kept() {
    let hub = hub().await;
    let admin = hub.admin();
    let alice = hub.person("Alice", "U-A");
    assert!(
        hub.lifecycle
            .delete(alice.id, &DeletePlan::default(), &admin, false)
            .await
            .unwrap()
            .complete
    );
    let a = alice.id.to_string();
    let left = [session(&hub, "dm", "U-A"), session(&hub, "web", &a)];
    spend(&hub, &a);

    let scan = hub.lifecycle.orphans(&admin).await.unwrap();
    let orphan = found(&scan, alice.id);
    assert_eq!(orphan.proof, Proof::Recorded);
    assert_eq!(orphan.names, ["U-A", "slack:U-A"]);
    assert_eq!(orphan.held, BTreeMap::from([(Category::Sessions, 2)]));

    let report = hub
        .lifecycle
        .delete_orphan(alice.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();

    assert!(report.complete, "{report:#?}");
    for session in &left {
        assert!(hub.service.sessions.get(&session.id).unwrap().is_none());
    }
    let spent = hub
        .service
        .telemetry_store
        .cost_by_identity(chrono::DateTime::<Utc>::UNIX_EPOCH, Utc::now())
        .unwrap();
    assert!(
        spent.contains(&(a.clone(), 1.0)),
        "billing is kept: {spent:?}"
    );
    assert!(hub
        .lifecycle
        .orphans(&admin)
        .await
        .unwrap()
        .orphans
        .is_empty());
    assert!(matches!(
        hub.lifecycle
            .delete_orphan(alice.id, &DeletePlan::default(), &admin, false)
            .await,
        Err(LifecycleError::NotFound(_))
    ));
    let kinds: Vec<String> = hub
        .lifecycle
        .operations(Some(alice.id), 10)
        .unwrap()
        .into_iter()
        .map(|op| op.kind)
        .collect();
    assert_eq!(kinds, ["orphan_delete", "delete"]);
}

/// The scan and the clean-up re-read who is live: a name a live identity
/// has taken on is theirs, and an id that is an identity again is refused,
/// before the operation and before each category under its lease.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_taken_on_by_a_live_identity_or_an_id_in_use_again_is_never_treated_as_gone() {
    let hub = hub().await;
    let admin = hub.admin();
    let alice = hub.person("Alice", "U-A");
    hub.lifecycle
        .delete(alice.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();
    let a = alice.id.to_string();
    let theirs = session(&hub, "dm", "U-A");
    let left = session(&hub, "web", &a);
    let carol = hub.person("Carol", "U-A");

    let scan = hub.lifecycle.orphans(&admin).await.unwrap();
    assert_eq!(
        found(&scan, alice.id).held,
        BTreeMap::from([(Category::Sessions, 1)])
    );
    assert!(found(&scan, alice.id).names.is_empty());

    let resumed = Person::orphan(alice.id, &["slack:U-A".to_string(), "U-A".to_string()]);
    assert_eq!(
        hub.lifecycle.still_orphaned(&resumed).unwrap().names,
        std::slice::from_ref(&a),
        "a name linked to someone live since is dropped mid-sweep"
    );
    assert!(
        hub.lifecycle
            .delete_orphan(alice.id, &DeletePlan::default(), &admin, false)
            .await
            .unwrap()
            .complete
    );
    assert!(hub.service.sessions.get(&left.id).unwrap().is_none());
    assert_eq!(
        hub.service.sessions.get(&theirs.id).unwrap().unwrap().owner,
        "U-A",
        "carol's now"
    );

    let lost = gone(&hub, "Lost", "U-L");
    spend(&hub, &lost.id.to_string());
    session(&hub, "dm", &lost.id.to_string());
    hub.identities.create(&lost).unwrap();
    assert!(matches!(
        hub.lifecycle.still_orphaned(&Person::orphan(lost.id, &[])),
        Err(LifecycleError::Refused(_))
    ));
    assert!(matches!(
        hub.lifecycle
            .delete_orphan(lost.id, &DeletePlan::default(), &admin, false)
            .await,
        Err(LifecycleError::Refused(_))
    ));
    assert!(hub
        .identities
        .get_by_linked_id("slack", "U-A")
        .unwrap()
        .is_some_and(|i| i.id == carol.id));
}

/// A run another replica is executing keeps the clean-up from finishing;
/// rerunning it resumes from the names its first pass recorded.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_clean_up_resumes_and_finishes() {
    let hub = hub().await;
    let admin = hub.admin();
    let lost = gone(&hub, "Lost", "U-L");
    let id = lost.id.to_string();
    spend(&hub, &id);
    let telemetry = Arc::clone(&hub.service.telemetry_store);
    telemetry
        .save_workflow_instance(&run(
            "r1",
            &format!("{id}/digest"),
            "running",
            serde_json::json!({"_run_as": id}),
        ))
        .unwrap();
    assert!(telemetry
        .try_claim_workflow_instance("r1", "another-replica", 600)
        .unwrap());
    session(&hub, "dm", &id);

    let first = hub
        .lifecycle
        .delete_orphan(lost.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();
    assert!(!first.complete);
    assert_eq!(
        Hub::category(&first, Category::WorkflowRuns)
            .tally
            .blocking
            .len(),
        1
    );

    telemetry
        .release_workflow_instance("r1", "another-replica")
        .unwrap();
    let second = hub
        .lifecycle
        .delete_orphan(lost.id, &DeletePlan::default(), &admin, false)
        .await
        .unwrap();

    assert!(second.complete, "{second:#?}");
    assert!(telemetry.get_workflow_instance("r1").unwrap().is_none());
    let statuses: Vec<OpStatus> = hub
        .lifecycle
        .operations(Some(lost.id), 10)
        .unwrap()
        .into_iter()
        .map(|op| op.status)
        .collect();
    assert_eq!(statuses, [OpStatus::Complete, OpStatus::Incomplete]);
}

#[test]
fn a_run_written_in_either_timestamp_form_is_paged_past() {
    let at = |raw: &str| super::automation::instance_time(raw).unwrap().timestamp();
    assert_eq!(at("2026-10-06T07:49:24+00:00"), at("2026-10-06 07:49:24"));
    assert!(super::automation::instance_time("yesterday").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_lease_refuses_a_second_clean_up_of_the_same_orphan() {
    let hub = hub().await;
    let admin = hub.admin();
    let lost = gone(&hub, "Lost", "U-L");
    spend(&hub, &lost.id.to_string());
    let conversation = session(&hub, "dm", &lost.id.to_string());
    let held = LifecycleOp {
        status: OpStatus::Running,
        lease_until: Utc::now() + Duration::minutes(5),
        finished_at: None,
        ..recorded(lost.id, &[])
    };
    assert!(hub.identities.claim_op(&held, Utc::now()).unwrap());

    let refused = hub
        .lifecycle
        .delete_orphan(lost.id, &DeletePlan::default(), &admin, false)
        .await;

    assert!(
        matches!(refused, Err(LifecycleError::Busy(_))),
        "{refused:?}"
    );
    assert!(hub
        .service
        .sessions
        .get(&conversation.id)
        .unwrap()
        .is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_id_nothing_proves_was_an_identity_is_left_alone() {
    let hub = hub().await;
    let admin = hub.admin();
    let stray = Uuid::new_v4();
    let conversation = session(&hub, "dm", &stray.to_string());

    let refused = hub
        .lifecycle
        .delete_orphan(stray, &DeletePlan::default(), &admin, false)
        .await;

    assert!(
        matches!(refused, Err(LifecycleError::Refused(_))),
        "{refused:?}"
    );
    assert!(hub
        .service
        .sessions
        .get(&conversation.id)
        .unwrap()
        .is_some());
    assert!(hub
        .lifecycle
        .operations(Some(stray), 10)
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_category_that_cannot_answer_leaves_the_scan_incomplete_and_the_rest_listed() {
    let hub = hub().await;
    let admin = hub.admin();
    let lost = gone(&hub, "Lost", "U-L");
    spend(&hub, &lost.id.to_string());
    session(&hub, "dm", &lost.id.to_string());
    let plans = workforce_common::learning_plan::learning_plans_dir(&hub.service.workspace);
    let _ = std::fs::remove_dir_all(&plans);
    std::fs::write(&plans, "not a directory").unwrap();

    let scan = hub.lifecycle.orphans(&admin).await.unwrap();

    assert!(!scan.complete);
    assert!(
        scan.failed.contains_key(&Category::LearningPlans),
        "{scan:#?}"
    );
    assert_eq!(found(&scan, lost.id).held[&Category::Sessions], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_live_identity_is_read_however_many_pages_they_take() {
    let hub = hub().await;
    let people: Vec<Identity> = (0..=2 * IDENTITY_PAGE)
        .map(|n| hub.person(&format!("P{n}"), &format!("U-{n}")))
        .collect();

    let live = hub.lifecycle.live_names().unwrap();

    for (n, person) in people.iter().enumerate() {
        assert!(live.contains(&person.id.to_string()), "{n}");
        assert!(live.contains(&format!("slack:u-{n}")), "{n}");
    }
}
