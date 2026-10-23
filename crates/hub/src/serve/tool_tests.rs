//! Tests of agent tools that need more than the tool: a service built
//! through composition, or the surfaces of the subsystems around it.

use std::sync::Arc;

use workforce_agent::tools::Tool;

use super::*;
use crate::compaction_tool::CompactionTool;
use crate::secret_tool::SecretListTool;

#[tokio::test]
async fn set_compaction_refuses_a_model_the_agent_would_not_boot_with() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = tmp.path().to_string_lossy().to_string();
    config.memory.db_path = std::path::Path::new(&config.hub.data_dir)
        .join("memory.db")
        .to_string_lossy()
        .into_owned();
    config.llm.api_key = Some("test-key".into());
    let service = AgentService::from_config(config, None).await.unwrap();
    let tool = CompactionTool::new(Arc::clone(&service));
    let config_toml = service.workspace.join("config.toml");

    let refused = tool
        .execute(serde_json::json!({"action": "set", "compaction_model": "codex:gpt-6.1-sol"}))
        .await
        .unwrap();
    assert!(!refused.success, "{refused:?}");
    assert!(!config_toml.exists());
    assert!(service.active_compaction.read().is_none());

    let set = tool
        .execute(serde_json::json!({"action": "set", "keep_recent_messages": 7}))
        .await
        .unwrap();
    assert!(set.success, "{set:?}");
    assert!(std::fs::read_to_string(&config_toml)
        .unwrap()
        .contains("keep_recent_messages = 7"));
}

/// The `/secret` slash command silently fails inside Slack threads (Slack
/// intercepts a leading `/`), so users keep being told to run a command
/// that does nothing. The thread-safe form is `cmd: secret …`, which works
/// on every surface. These strings reach the user on every agent (tool
/// descriptions) or directly (the Claude Code setup messages), regardless
/// of any workspace `AGENTS.md`, so they are the load-bearing surfaces.
/// This guard keeps any of them from reintroducing the broken instruction.
#[test]
fn no_runtime_surface_recommends_the_secret_slash_command() {
    use workforce_common::subscription::{CredentialTier, Payer, SubscriptionKind};
    let claude_no_login = crate::agent_turn::connect::no_login_notice(
        "allan",
        &crate::hub_core::logins::resolve::NoLogin {
            kind: SubscriptionKind::Claude,
            payer: Payer::Person(uuid::Uuid::nil()),
            misses: vec![(
                CredentialTier::AgentUser,
                crate::hub_core::logins::resolve::Miss::Absent,
            )],
        },
    );
    let surfaces: [(&str, String); 4] = [
        (
            "secret.list description",
            SecretListTool.description().to_string(),
        ),
        (
            "claude_code.setup_token description",
            crate::claude_code_setup_token_tool::DESCRIPTION.to_string(),
        ),
        ("no-login notice", claude_no_login.clone()),
        (
            "engine_unavailable_blocks body",
            serde_json::to_string(
                &crate::slack_blocks::engine_unavailable_blocks(&claude_no_login).1,
            )
            .unwrap(),
        ),
    ];
    for (label, text) in surfaces {
        assert!(
            !text.contains("/secret"),
            "{label} must never recommend the `/secret` slash command (it does not work in \
             Slack threads); use `cmd: secret <name> <value>` instead"
        );
    }
}
