//! Moves and handovers on a hub (boss-gaps W8): a boss moved to another
//! group keeps what it remembered to the group it left, recalls none of
//! it there, and replaces the boss there only when the person says so;
//! `crew.handover` writes only a note its boss was asked for; and the note
//! the person sends is the one thing the new boss is given.

use super::*;
use workforce_common::oversight::Scope;
use workforce_desk::moves::{Choices, Item};
use workforce_desk::{MoveAsk, MoveCommit};
use workforce_memory::group_tagged::group_tag;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer};

const ACTS: &str = "level = \"act\"\n";

fn filter() -> MemoryFilter {
    MemoryFilter {
        system_unscoped: true,
        limit: Some(20),
        ..MemoryFilter::default()
    }
}

async fn recalled(service: &AgentService, q: &str) -> Vec<String> {
    let mut found: Vec<String> = service
        .memory_store
        .as_ref()
        .unwrap()
        .list(filter())
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.content.contains(q))
        .map(|m| m.content)
        .collect();
    found.sort();
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn a_moved_boss_keeps_its_old_groups_memories_out_of_the_new_one_and_replaces_only_when_told()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let (mia, l3) = agent(dir.path(), "mia", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        // Sarah, before she oversaw anything: what she learned then is untagged.
        let raw = sarah.memory_store.clone().unwrap();
        raw.remember(Memory::new(
            "acmecanary from before she was a boss",
            MemoryLayer::Semantic,
            sarah.agent_id.clone(),
        ))
        .await
        .unwrap();
        write_config(
            &sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        );
        write_config(
            &mia,
            &format!("[oversight]\nenabled = true\ngroup = \"atlas\"\n{ACTS}"),
        );
        let all = [&desk_boss, &sarah, &mia];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("three bosses", async || {
            (d.bosses().len() == 3).then_some(())
        })
        .await;

        // As Acme's boss, what she remembers is Acme's, and she reads it.
        raw.remember(Memory::new(
            "acmecanary learned in Acme",
            MemoryLayer::Semantic,
            sarah.agent_id.clone(),
        ))
        .await
        .unwrap();
        assert_eq!(
            recalled(&sarah, "acmecanary").await,
            [
                "acmecanary from before she was a boss",
                "acmecanary learned in Acme"
            ]
        );
        let tagged = raw.list(filter()).await.unwrap();
        assert!(tagged
            .iter()
            .any(|m| m.content == "acmecanary learned in Acme"
                && m.tags.contains(&group_tag(Some("acme")))));

        // Her DM with the person holds Acme's talk.
        let mut dm = sarah
            .sessions
            .get_or_create(DESK_CHANNEL, PERSON, "sarah")
            .unwrap();
        dm.add_user_message("acmecanary: the Acme pricing", None);
        sarah.sessions.save(&dm).unwrap();

        // Into Atlas, which has Mia: refused until the person says replace.
        let refused =
            workforce_desk::crew::Crew::move_boss(crew.as_ref(), "sarah", Some("atlas"), false)
                .await;
        assert!(refused.unwrap_err().contains("Mia already oversees it"));
        assert_eq!(d.scope_of_boss("sarah"), Some(Scope::Group("acme".into())));
        workforce_desk::crew::Crew::move_boss(crew.as_ref(), "sarah", Some("atlas"), true)
            .await
            .unwrap();
        eventually("Sarah oversees Atlas", async || {
            (d.scope_of_boss("sarah") == Some(Scope::Group("atlas".into()))).then_some(())
        })
        .await;
        assert!(d.scope_of_boss("mia").is_none(), "Mia stops overseeing");
        // Her DM goes on fresh: Acme's talk stays behind, unread.
        let now = sarah.sessions.find(DESK_CHANNEL, PERSON).unwrap();
        assert!(now.as_ref().is_none_or(|s| s.id != dm.id));
        assert!(now
            .iter()
            .flat_map(|s| s.messages.iter())
            .all(|m| !m.content.contains("acmecanary")));
        let old = sarah.sessions.get(&dm.id).unwrap().unwrap();
        assert!(old.is_sealed() && crate::memory_fence::left_behind(&old));
        // Nothing of Acme's, tagged or from before, reaches her in Atlas.
        assert!(recalled(&sarah, "acmecanary").await.is_empty());
        assert!(raw
            .list(filter())
            .await
            .unwrap()
            .iter()
            .filter(|m| m.content.contains("acmecanary"))
            .all(|m| m.tags.contains(&group_tag(Some("acme")))));
        // Nor the main boss's, nor Mia's, reach anyone else.
        assert!(recalled(&mia, "acmecanary").await.is_empty());
        assert!(recalled(&desk_boss, "acmecanary").await.is_empty());
        // Acme has no boss now: the main boss sees it from now on.
        assert!(!d.bossed().contains("acme"));
        drop((l1, l2, l3));
    })
    .await;
}

/// `crew.handover` writes only a note its boss was asked for, the note the
/// person sends is the one delivered, and the new boss gets it in one turn.
#[tokio::test(flavor = "multi_thread")]
async fn a_handover_is_drafted_only_by_its_old_boss_and_only_the_persons_note_reaches_the_new_one()
{
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let (mia, l3) = agent(dir.path(), "mia", "ok").await;
        let d = desk_with(
            pool,
            dir.path(),
            workforce_desk::Floor {
                redactor: Some(Arc::new(crate::desk_memory::HubRedactor)),
                ..Default::default()
            },
        )
        .await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        let s = new_session(&d, dir.path(), "acme-api").await;
        d.change_layout(|l| l.put(&workforce_desk::session_item(s.id), "acme", None))
            .await
            .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        write_config(
            &sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        );
        write_config(
            &mia,
            &format!("[oversight]\nenabled = true\ngroup = \"atlas\"\n{ACTS}"),
        );
        let all = [&desk_boss, &sarah, &mia];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        for boss in [&desk_boss, &sarah, &mia] {
            let dm = boss
                .sessions
                .get_or_create(DESK_CHANNEL, PERSON, boss.agent_id.as_str())
                .unwrap();
            boss.sessions.save(&dm).unwrap();
            eventually("each boss's tools", async || {
                boss.tools.has("crew.handover").then_some(())
            })
            .await;
        }
        let p = d
            .preview_move(MoveAsk {
                item: Item::Session(s.id),
                to: Some("atlas".into()),
                choices: Choices::default(),
            })
            .await
            .unwrap();
        let h = p.handover.clone().expect("a note");
        assert!(h.drafting, "Sarah is asked");
        // Sarah's handover turn ran on its own channel.
        eventually("Sarah's handover turn", async || {
            sarah
                .sessions
                .list(Some(&super::super::handover_channel(h.handover.id)), 1)
                .ok()
                .filter(|l| !l.is_empty())
                .map(drop)
        })
        .await;
        let id = h.handover.id.to_string();
        let mia_tool = mia.tools.get("crew.handover").unwrap();
        let r = mia_tool
            .execute(json!({ "handover_id": id, "note": "Mia wrote this" }))
            .await
            .unwrap();
        assert_eq!(r.error.as_deref(), Some("That isn't one of Atlas's."));
        let sarah_tool = sarah.tools.get("crew.handover").unwrap();
        let secret = format!("key {}{}", "sk-ant-api03-", "a".repeat(90));
        let r = sarah_tool
            .execute(json!({ "handover_id": id, "note": secret }))
            .await
            .unwrap();
        assert!(!r.success, "a secret is refused: {:?}", r.error);
        let r = sarah_tool
            .execute(json!({ "handover_id": id, "note": "Port 8080; the fix is half done." }))
            .await
            .unwrap();
        assert!(r.success, "{:?}", r.error);
        assert_eq!(
            d.handover_view(h.handover.id).await.unwrap().handover.note,
            "Port 8080; the fix is half done."
        );
        // Nothing reached Mia yet.
        let line = super::super::line::line_channel(s.id);
        assert!(mia.sessions.list(Some(&line), 1).unwrap().is_empty());
        d.commit_move(MoveCommit {
            preview: p.id,
            choices: Choices::default(),
            note: Some("Port 8080, from the person.".into()),
            notes: vec![],
        })
        .await
        .unwrap();
        let got = eventually("Mia's handover", async || {
            let sessions = mia.sessions.list(Some(&line), 1).ok()?;
            let session = sessions.into_iter().next()?;
            let full = mia.sessions.get(&session.id).ok()??;
            full.messages
                .iter()
                .find(|m| m.content.contains("[Desk handover]"))
                .map(|m| m.content.clone())
        })
        .await;
        assert!(got.contains("Port 8080, from the person."), "{got}");
        assert!(!got.contains("half done"), "never the boss's draft: {got}");
        // Sarah's drafting turn is hers alone.
        assert!(mia
            .sessions
            .list(Some(&super::super::handover_channel(h.handover.id)), 1)
            .unwrap()
            .is_empty());
        drop((l1, l2, l3));
    })
    .await;
}

/// A boss's turn that spans a move: the move waits for it within a bound
/// and is refused past it, and what the turn remembers after is still the
/// scope it began in.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_spans_a_move_keeps_its_scope_and_the_move_waits_for_it() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        write_config(
            &sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        );
        let all = [&desk_boss, &sarah];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("two bosses", async || (d.bosses().len() == 2).then_some(())).await;

        // A turn of Sarah's begins as Acme's boss.
        let began = crate::memory_fence::turn_fence(&sarah.agent_id, &sarah.workspace);
        let now = chrono::Utc::now();
        crew.running.lock().insert(
            "turn-1".into(),
            super::super::Running {
                agent_id: "sarah".into(),
                task: None,
                detail: None,
                since: now,
                last_activity: now,
            },
        );
        let refused = crew.move_boss_to("sarah", Some("atlas"), false).await;
        assert!(refused.unwrap_err().contains("middle of a turn"));
        assert_eq!(d.scope_of_boss("sarah"), Some(Scope::Group("acme".into())));
        // It ends while the move waits: the move goes on.
        let ending = {
            let crew = Arc::clone(&crew);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                crew.running.lock().remove("turn-1");
            })
        };
        crew.move_boss_to("sarah", Some("atlas"), false)
            .await
            .unwrap();
        ending.await.unwrap();
        eventually("Sarah oversees Atlas", async || {
            (d.scope_of_boss("sarah") == Some(Scope::Group("atlas".into()))).then_some(())
        })
        .await;
        // Its passive extraction, run after the move, is still Acme's.
        let store = sarah.memory_store.clone().unwrap();
        let mut m = Memory::new(
            "acmecanary from the turn",
            MemoryLayer::Episodic,
            sarah.agent_id.clone(),
        );
        m.identity_id = Some(PERSON.into());
        let id = workforce_agent::runner::CURRENT_MEMORY_FENCE
            .scope(began, store.remember(m))
            .await
            .unwrap();
        let listed = store.list(filter()).await.unwrap();
        assert!(
            listed.iter().all(|m| m.id != id),
            "an Acme memory never reaches her in Atlas"
        );
        drop((l1, l2));
    })
    .await;
}

/// An agent made a boss keeps what it learned outside the group out of it.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_made_a_groups_boss_brings_nothing_it_learned_outside_the_group() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| l.create("acme", "Acme", &[], None))
            .await
            .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        write_config(&sarah, "");
        let all = [&desk_boss, &sarah];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let _crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("the main boss", async || {
            (!d.bosses().is_empty()).then_some(())
        })
        .await;
        let store = sarah.memory_store.clone().unwrap();
        let mut m = Memory::new(
            "globexcanary learned ungrouped",
            MemoryLayer::Semantic,
            sarah.agent_id.clone(),
        );
        m.identity_id = Some(PERSON.into());
        store.remember(m).await.unwrap();
        assert_eq!(recalled(&sarah, "globexcanary").await.len(), 1);
        let current = std::fs::read_to_string(
            sarah
                .workspace
                .join(workforce_common::config_files::WORKSPACE_CONFIG),
        )
        .unwrap();
        crate::agent_config_write::save(
            &sarah,
            "sarah",
            None,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
            workforce_orchestrator::workflow_store::EditContext::new("oversight", None),
            |now| now == Some(current.as_str()),
        )
        .await
        .unwrap();
        assert!(
            recalled(&sarah, "globexcanary").await.is_empty(),
            "kept with everything ungrouped, out of Acme"
        );
        drop((l1, l2));
    })
    .await;
}

/// The mock model answers slowly from now on: a turn of the agent is in
/// flight for `delay`.
async fn slowly(server: &wiremock::MockServer, delay: std::time::Duration) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "c1", "object": "chat.completion", "model": "mock",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                }))
                .set_delay(delay),
        )
        .with_priority(1)
        .mount(server)
        .await;
}

/// A heartbeat turn (not one Desk's crew tracks) that spans a move: the
/// move waits for it, and is refused while it runs past the bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_turn_that_spans_a_move_holds_the_move_until_it_ends() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        write_config(
            &sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        );
        let all = [&desk_boss, &sarah];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("two bosses", async || (d.bosses().len() == 2).then_some(())).await;
        slowly(&l2, std::time::Duration::from_millis(1500)).await;
        let heartbeat = {
            let sarah = Arc::clone(&sarah);
            tokio::spawn(async move {
                sarah
                    .process_message(
                        "heartbeat",
                        PERSON,
                        "Check on things.",
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await
            })
        };
        eventually("the heartbeat in flight", async || {
            (crate::memory_fence::turns_in_flight(&sarah.workspace) > 0).then_some(())
        })
        .await;
        let refused = crew.move_boss_to("sarah", Some("atlas"), false).await;
        assert!(refused.unwrap_err().contains("middle of a turn"));
        assert_eq!(d.scope_of_boss("sarah"), Some(Scope::Group("acme".into())));
        let _ = heartbeat.await.unwrap();
        assert_eq!(crate::memory_fence::turns_in_flight(&sarah.workspace), 0);
        crew.move_boss_to("sarah", Some("atlas"), false)
            .await
            .unwrap();
        drop((l1, l2));
    })
    .await;
}

/// A room turn, by its id or by an older Desk's slug, reaches only an
/// agent the room's group lets in now.
#[tokio::test(flavor = "multi_thread")]
async fn a_room_turn_by_id_or_slug_reaches_a_boss_only_inside_the_rooms_group() {
    use workforce_common::channels::desk::DeskPlace;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (desk_boss, l1) = agent(dir.path(), "desk", "ok").await;
        let (sarah, l2) = agent(dir.path(), "sarah", "ok").await;
        let d = desk(pool, dir.path()).await;
        d.change_layout(|l| {
            l.create("acme", "Acme", &[], None)?
                .create("atlas", "Atlas", &[], None)
        })
        .await
        .unwrap();
        write_config(&desk_boss, &format!("[oversight]\nenabled = true\n{ACTS}"));
        write_config(
            &sarah,
            &format!("[oversight]\nenabled = true\ngroup = \"acme\"\n{ACTS}"),
        );
        let all = [&desk_boss, &sarah];
        let state = hub(&all.iter().map(|s| Arc::clone(s)).collect::<Vec<_>>());
        let crew = CrewHub::start(state, d.clone(), FAST, CancellationToken::new());
        eventually("two bosses", async || (d.bosses().len() == 2).then_some(())).await;
        let room = d
            .create_room(workforce_desk::rooms::NewRoom {
                name: "Acme launch".into(),
                members: vec![workforce_desk::rooms::Member {
                    hub: workforce_desk::rooms::LOCAL_HUB.into(),
                    agent: "sarah".into(),
                }],
                default_member: None,
                group: Some("acme".into()),
                share_with: vec![],
            })
            .await
            .unwrap();
        let by_id = DeskPlace::Room {
            room: room.id,
            name: room.name.clone(),
            thread: None,
        };
        let by_slug = DeskPlace::LegacyRoom {
            slug: room.slug.clone(),
        };
        let by_old_id = DeskPlace::LegacyRoom {
            slug: room.id.to_string(),
        };
        for place in [&by_id, &by_slug, &by_old_id] {
            assert!(super::super::room_turn_refused(&sarah, place)
                .await
                .is_none());
        }
        crew.move_boss_to("sarah", Some("atlas"), false)
            .await
            .unwrap();
        eventually("Sarah oversees Atlas", async || {
            (d.scope_of_boss("sarah") == Some(Scope::Group("atlas".into()))).then_some(())
        })
        .await;
        for place in [&by_id, &by_slug, &by_old_id] {
            assert!(
                super::super::room_turn_refused(&sarah, place)
                    .await
                    .is_some(),
                "{place:?}"
            );
        }
        // A room no desk here keeps is its own Desk's to bound.
        let elsewhere = DeskPlace::LegacyRoom {
            slug: "someone-elses-room".into(),
        };
        assert!(super::super::room_turn_refused(&sarah, &elsewhere)
            .await
            .is_none());
        drop((l1, l2));
    })
    .await;
}
