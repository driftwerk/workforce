//! `cmd: workflow run <id> [key=value …]` — start a workflow from chat as the
//! caller, with the current channel as its delivery channel. Human-only by
//! construction (the agent cannot emit a `cmd:`), and deterministic: the
//! workflow's own report is what reaches the channel, not a model's retelling
//! of a tool result.
//!
//! Every other `cmd: workflow` subcommand is who-may-reach-it, which reads the
//! same for workflows, playbooks and monitors — so it lives once, in
//! [`crate::def_cmd`], and this module hands it the line.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use workforce_agent::runner::CURRENT_USER_ID;

use crate::commands::CommandResult;
use crate::def_cmd::DefNoun;
use crate::endpoint_cmd::{identity_id, split_word};
use crate::serve::AgentService;

/// Boxed and erased: a workflow step can run a chat turn, which can run this
/// command, so the command's future must not appear in its own type.
pub fn execute_workflow<'a>(
    service: &'a Arc<AgentService>,
    channel: &'a str,
    user: &'a str,
    args: Option<&'a str>,
) -> Pin<Box<dyn Future<Output = CommandResult> + Send + 'a>> {
    Box::pin(async move {
        let (sub, rest) = split_word(args.unwrap_or(""));
        match sub {
            "run" | "start" => run(service, channel, user, rest).await,
            _ => crate::def_cmd::execute_def(service, DefNoun::Workflow, user, args).await,
        }
    })
}

async fn run(service: &Arc<AgentService>, channel: &str, user: &str, args: &str) -> CommandResult {
    let (workflow_id, rest) = split_word(args);
    if workflow_id.is_empty() {
        return CommandResult::info(crate::def_cmd::usage(DefNoun::Workflow));
    }
    let Some(uid) = identity_id(user) else {
        return CommandResult::info("Only a signed-in user can start a workflow.");
    };
    let Some(runtime) = service.workflow_runtime() else {
        return CommandResult::info("Workflows are not available on this hub.");
    };
    let key = match runtime.definition_ownership(workflow_id, Some(&uid)).await {
        Some(_) => crate::workflow_runtime::def_key(Some(&uid), workflow_id),
        None => workflow_id.to_string(),
    };
    let (channel_id, thread_id) = delivery_target(channel);
    let mut payload = context_payload(rest);
    payload
        .entry("channel_id")
        .or_insert_with(|| serde_json::Value::String(channel_id.clone()));
    let started = CURRENT_USER_ID
        .scope(
            uid,
            Arc::clone(runtime).start_workflow(
                &key,
                serde_json::Value::Object(payload),
                Some(channel_id),
                thread_id,
                None,
                None,
            ),
        )
        .await;
    match started {
        Ok(instance_id) => CommandResult::info(format!(
            "▶️ Started `{workflow_id}` (instance {}) — its report posts here when it finishes.",
            instance_id.chars().take(8).collect::<String>()
        )),
        Err(e) => CommandResult::info(format!("Could not start `{workflow_id}`: {e}")),
    }
}

/// Where the report posts. A command arrives with its session key, which on
/// Slack is `slack:<workspace>:<channel>:<thread>`: the workflow needs the
/// channel and thread inside it, not the key. Any other shape is a channel.
fn delivery_target(channel: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = channel.splitn(4, ':').collect();
    match parts.as_slice() {
        ["slack", _, slack_channel, thread] => (
            slack_channel.to_string(),
            Some(thread.to_string()).filter(|t| !t.is_empty()),
        ),
        _ => (channel.to_string(), None),
    }
}

/// `key=value` words as a JSON object; a word without `=` is ignored.
fn context_payload(args: &str) -> serde_json::Map<String, serde_json::Value> {
    args.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, value)| {
            (
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slack_session_key_yields_its_channel_and_thread() {
        assert_eq!(
            delivery_target("slack:acme:D0FAKEDMAA2:1789036738.343109"),
            (
                "D0FAKEDMAA2".to_string(),
                Some("1789036738.343109".to_string())
            )
        );
        assert_eq!(
            delivery_target("slack:acme:C0FAKECHAN2:"),
            ("C0FAKECHAN2".to_string(), None)
        );
        assert_eq!(
            delivery_target("C0FAKECHAN2"),
            ("C0FAKECHAN2".to_string(), None)
        );
    }

    #[test]
    fn context_pairs_become_string_payload_fields() {
        let payload = context_payload("days=7 project=atlas stray =empty");
        assert_eq!(payload["days"], "7");
        assert_eq!(payload["project"], "atlas");
        assert_eq!(payload.len(), 2);
        assert!(context_payload("").is_empty());
    }
}
