use std::sync::Arc;

use parking_lot::Mutex;
use workforce_agent::runner::SessionContext;
use workforce_agent::tools::Tool as _;
use workforce_common::channels::desk::DeskScope;
use workforce_common::channels::{
    ChannelCapabilities, ChannelSurface, Reaction, SentMessage, SurfaceKey,
};

use super::*;

#[test]
fn a_turn_s_conversation_is_the_one_its_session_key_names() {
    let slack = |workspace: &str, channel: &str, thread: Option<&str>| Conversation {
        platform: "slack".into(),
        workspace: Some(workspace.into()),
        channel: channel.into(),
        thread: thread.map(Into::into),
    };
    // A thread's key names its thread, whatever the turn says.
    assert_eq!(
        Conversation::of_turn("slack:T1:D1:1700.1", Some("D1"), Some("1800.2")),
        Some(slack("T1", "D1", Some("1700.1")))
    );
    // A DM or channel's key names none: the thread the turn answers in.
    assert_eq!(
        Conversation::of_turn("slack:T1:D1", Some("D1"), Some("1700.1")),
        Some(slack("T1", "D1", Some("1700.1")))
    );
    assert_eq!(
        Conversation::of_turn("slack:T1:C1", Some("C2"), Some("1700.1")),
        Some(slack("T1", "C1", None)),
        "another channel's thread is not this conversation's"
    );
    assert_eq!(
        Conversation::of_turn("slack:T1:C1", None, None),
        Some(slack("T1", "C1", None))
    );
    assert_eq!(
        Conversation::of_turn("teams:19:abc@thread.v2", None, None),
        Some(Conversation {
            platform: "teams".into(),
            workspace: None,
            channel: "teams:19:abc@thread.v2".into(),
            thread: None,
        })
    );
    for none in [
        "desk",
        "desk:planning",
        "web:alice",
        "scheduler:job-1",
        "slack:T1:C1:slash",
        "slack::C1",
        "slack:T1",
        "teams:",
    ] {
        assert_eq!(
            Conversation::of_turn(none, Some("C1"), Some("1.0")),
            None,
            "{none}"
        );
    }
}

/// A surface that keeps what it is sent, or refuses everything.
#[derive(Default)]
struct Posts {
    refuse: bool,
    sent: Mutex<Vec<(String, Option<String>, String)>>,
}

#[async_trait::async_trait]
impl ChannelSurface for Posts {
    fn platform(&self) -> &str {
        "slack"
    }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }
    async fn add_reaction(&self, _: &Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn remove_reaction(&self, _: &Reaction) -> eyre::Result<()> {
        Ok(())
    }
    async fn send_message(&self, message: &OutboundMessage) -> eyre::Result<SentMessage> {
        if self.refuse {
            eyre::bail!("channel_not_found");
        }
        self.sent.lock().push((
            message.channel.clone(),
            message.thread_id.clone(),
            message.text.clone(),
        ));
        Ok(SentMessage {
            channel: message.channel.clone(),
            message_id: "1800.1".into(),
            thread_id: message.thread_id.clone(),
            platform: "slack".into(),
        })
    }
}

const WAS: &str = "[skills]\nenabled = [\"jira\"]\nread_only = [\"jira:globex\"]\n\n[skills.connections]\njira = [\"acme\", \"globex\"]\n";

/// Scout, on a hub that runs Desk, its Slack workspace `T1` posting
/// through `surface` when there is one, its config one a change to can
/// widen.
async fn scout(
    dir: &std::path::Path,
    surface: Option<Arc<Posts>>,
) -> (Arc<AgentService>, wiremock::MockServer) {
    use crate::serve::Compose as _;
    let (config, server) = crate::serve::testing::mock_llm_config(&dir.join("scout"), |c| {
        c.agents = vec![workforce_common::settings::AgentConfig {
            id: "scout".into(),
            ..Default::default()
        }];
        c.desk.enabled = true;
    })
    .await;
    let service = AgentService::from_config(config, None).await.unwrap();
    std::fs::create_dir_all(&service.workspace).unwrap();
    // Saved as the person saves it, so the hub knows the file as its own.
    crate::agent_config_write::save(
        &service,
        "scout",
        None,
        WAS,
        workforce_orchestrator::workflow_store::EditContext::new("web", None),
        |_| true,
    )
    .await
    .unwrap();
    if let Some(surface) = surface {
        service
            .register_surface(SurfaceKey::workspace("slack", "T1"), surface)
            .await;
    }
    (service, server)
}

/// The agent widening its own connectors, as its gate sees the call and
/// then runs it.
async fn widen(service: &Arc<AgentService>) {
    let tool = crate::config_tool::ConfigTool::new(Arc::clone(service), None);
    let input = serde_json::json!({"action": "set", "key": "skills.read_only", "value": []});
    tool.effect(&input);
    let result = tool.execute(input).await.unwrap();
    assert!(result.success, "{result:?}");
}

/// A person's message in Slack thread `1700.1` of their DM `D1` with
/// scout, as the Slack dispatch loop runs its turn.
fn in_slack(person: &str) -> SessionContext {
    SessionContext {
        key: Some("slack:T1:D1:1700.1".into()),
        id: Some(uuid::Uuid::new_v4().to_string()),
        user_id: Some(person.into()),
        channel_id: Some("D1".into()),
        thread_id: Some("1700.1".into()),
        workspace_id: Some("T1".into()),
        agent_id: Some("scout".into()),
        chat: true,
        ..Default::default()
    }
}

/// How many of the agent's messages in `person`'s Desk DM with scout are
/// unread, read as Desk's badge reads them.
fn dm_unread(service: &AgentService, person: &str) -> i64 {
    crate::desk_reads::dm_unread(
        &*service.sessions,
        "scout",
        person,
        chrono::DateTime::<chrono::Utc>::MIN_UTC,
    )
    .unwrap()
    .1
    .count
}

const TOLD: &str = "I changed my own connectors, so I reach more now: jira: may now change \
                    things through `globex`. Change it back in my Connectors.";

#[tokio::test(flavor = "multi_thread")]
async fn a_slack_turn_that_widens_its_connectors_says_so_in_slack_and_leaves_desk_unread_alone() {
    let dir = tempfile::tempdir().unwrap();
    let posts = Arc::new(Posts::default());
    let (scout, _llm) = scout(dir.path(), Some(Arc::clone(&posts))).await;
    let person = uuid::Uuid::new_v4().to_string();
    let before = dm_unread(&scout, &person);

    in_slack(&person).scope(widen(&scout)).await;

    assert_eq!(
        *posts.sent.lock(),
        [(
            "D1".to_string(),
            Some("1700.1".to_string()),
            TOLD.to_string()
        )]
    );
    assert_eq!(dm_unread(&scout, &person), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_desk_turn_that_widens_its_connectors_says_so_in_the_desk_dm_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let posts = Arc::new(Posts::default());
    let (scout, _llm) = scout(dir.path(), Some(Arc::clone(&posts))).await;
    let person = uuid::Uuid::new_v4().to_string();
    let before = dm_unread(&scout, &person);
    let on_desk = SessionContext {
        key: Some(DESK_CHANNEL.into()),
        channel_id: None,
        thread_id: None,
        workspace_id: None,
        desk: Some(DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm { thread: None }, DESK_CHANNEL, None, false),
            attended: true,
            relayed: false,
            owner: person.clone(),
        }),
        ..in_slack(&person)
    };

    on_desk.scope(widen(&scout)).await;

    assert!(posts.sent.lock().is_empty());
    assert_eq!(dm_unread(&scout, &person), before + 1);
    let dm = scout.sessions.find(DESK_CHANNEL, &person).unwrap().unwrap();
    let last = dm.messages.last().unwrap();
    assert_eq!(last.content, TOLD);
    assert_eq!(
        last.desk.as_ref().and_then(|mark| mark.delivery),
        Some(DeliveryKind::Notice)
    );
}

/// A Slack conversation that can't take the notice (its post refused, or
/// no surface for its workspace now) leaves it in the Desk DM, unread.
#[tokio::test(flavor = "multi_thread")]
async fn a_notice_slack_cant_take_is_left_in_the_desk_dm_unread() {
    for surface in [
        Some(Arc::new(Posts {
            refuse: true,
            ..Default::default()
        })),
        None,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let gone = surface.is_none();
        let (scout, _llm) = scout(dir.path(), surface.clone()).await;
        let person = uuid::Uuid::new_v4().to_string();
        let before = dm_unread(&scout, &person);

        in_slack(&person).scope(widen(&scout)).await;

        assert_eq!(
            dm_unread(&scout, &person),
            before + 1,
            "surface gone: {gone}"
        );
        let dm = scout.sessions.find(DESK_CHANNEL, &person).unwrap().unwrap();
        assert_eq!(dm.messages.last().unwrap().content, TOLD);
    }
}

/// A notice no person's message in Slack is behind (a job's run, though it
/// is had in a Slack thread) or one for someone else goes to the Desk DM,
/// and Slack is told nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_notice_no_slack_turn_of_its_person_is_behind_goes_to_the_desk_dm() {
    let dir = tempfile::tempdir().unwrap();
    let posts = Arc::new(Posts::default());
    let (scout, _llm) = scout(dir.path(), Some(Arc::clone(&posts))).await;
    let person = uuid::Uuid::new_v4().to_string();
    let scheduled = SessionContext {
        automated_origin: Some("scheduler".into()),
        chat: false,
        ..in_slack(&person)
    };
    let not_a_chat = SessionContext {
        chat: false,
        ..in_slack(&person)
    };
    let another_agent_s = SessionContext {
        agent_id: Some("mia".into()),
        ..in_slack(&person)
    };
    let mut unread = dm_unread(&scout, &person);
    for turn in [scheduled, not_a_chat, another_agent_s] {
        let told = turn
            .scope(notice(&scout, &person, "a note".to_string()))
            .await
            .unwrap();
        assert_eq!(told, Told::InDeskDm);
        unread += 1;
        assert_eq!(dm_unread(&scout, &person), unread);
    }
    // Someone else's, said in this person's Slack turn.
    let theirs = uuid::Uuid::new_v4().to_string();
    let told = in_slack(&person)
        .scope(notice(&scout, &theirs, "a note".to_string()))
        .await
        .unwrap();
    assert_eq!(told, Told::InDeskDm);
    assert_eq!(dm_unread(&scout, &theirs), 1);
    assert!(posts.sent.lock().is_empty());
}
