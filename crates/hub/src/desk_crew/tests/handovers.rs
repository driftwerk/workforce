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
