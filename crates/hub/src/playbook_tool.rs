//! Agent-callable tool for designing, managing, and starting playbooks.
//!
//! Actions: `list`, `get`, `schema`, `validate`, `save`, `delete`, `start`,
//! `continue`, and `exit`. `start`/`continue`/`exit` only *request* the change:
//! the actual playbook state is set in the dispatch layer (a tool runs mid-turn
//! and its session writes would be clobbered by the turn's end-of-turn save), so
//! the Slack handler re-injects `cmd: playbook start <id>` / `cmd: continue` /
//! `cmd: exit` after this turn — the same mechanism the buttons use. The user
//! still drives the flow by default via the buttons; the agent should only
//! continue/exit itself sparingly (see the action descriptions).

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_orchestrator::playbook::PlaybookDefinition;
use workforce_orchestrator::workflow_store::KIND_PLAYBOOK;

use crate::def_write_auth::{
    write_visibility, DefOp, DefWriteDenied, DefWriteGrant, TurnDefWriteAuth,
};
use crate::playbook_runtime::PlaybookRuntime;
use crate::resource_access::DefListing;

/// The identity the current turn runs as (`None` when unbound), used to
/// resolve and list playbooks the same way the interactive Slack path will.
fn current_turn_user() -> Option<String> {
    crate::user_config::current_identity_id()
}

/// The `owner` an action names, if any.
fn requested_owner(input: &PlaybookInput) -> Option<String> {
    input.owner.clone().filter(|o| !o.is_empty())
}

pub struct PlaybookTool {
    runtime: Arc<PlaybookRuntime>,
    /// Who may save or delete which definition row, for the turn's user — the
    /// same rule the gateway applies. The bare default treats every turn as a
    /// member's.
    write_auth: TurnDefWriteAuth,
}

impl PlaybookTool {
    pub fn new(runtime: Arc<PlaybookRuntime>) -> Self {
        Self {
            runtime,
            write_auth: TurnDefWriteAuth::default(),
        }
    }

    pub fn with_write_auth(mut self, write_auth: TurnDefWriteAuth) -> Self {
        self.write_auth = write_auth;
        self
    }
}

#[derive(Deserialize)]
struct PlaybookInput {
    action: String,
    #[serde(default)]
    playbook_id: Option<String>,
    #[serde(default)]
    toml_content: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
}

#[async_trait]
impl Tool for PlaybookTool {
    fn name(&self) -> &str {
        "playbook"
    }

    fn description(&self) -> &str {
        "Design, manage, run, and steer interactive playbooks: list, get, schema, validate, save, \
         delete, start, continue, exit. Use 'start' when the user asks you to run/start a playbook. \
         The user normally drives the flow with the Continue/Exit/Finish buttons — use 'continue' \
         and 'exit' only SPARINGLY: when the user has explicitly told you to advance/stop, or when \
         the current step's goal is unambiguously met and the user has signalled to proceed. \
         Otherwise let the user click. Always tell the user when you advance or exit on their behalf."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "get", "schema", "validate", "save", "delete", "start", "continue", "exit"],
                    "description": "Action to perform. 'continue'/'exit' steer the ACTIVE playbook \
                        and need no playbook_id; use them sparingly (the user usually clicks)."
                },
                "playbook_id": {
                    "type": "string",
                    "description": "Playbook id (required for 'get', 'save', 'delete', 'start')"
                },
                "toml_content": {
                    "type": "string",
                    "description": "Raw TOML playbook definition (for 'validate' and 'save'). \
                        Call action='schema' first to see the format."
                },
                "owner": {
                    "type": "string",
                    "description": "Identity id of the playbook's owner, to read, edit or delete \
                        another member's playbook that was shared with you (for 'get', 'save' \
                        and 'delete'). Omit for your own — or, as an administrator, the shared \
                        system playbook."
                },
                "visibility": {
                    "type": "string",
                    "enum": ["private", "team", "global"],
                    "description": "Who may see and start a playbook you own (for 'save'): 'private' \
                        (the default), 'team' — anyone in a team with you — or 'global' — anyone. \
                        Only the owner may change it."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: PlaybookInput = serde_json::from_value(input)
            .map_err(|e| eyre::eyre!("Invalid playbook input: {}", e))?;

        match input.action.as_str() {
            "list" => Ok(self.handle_list(start).await),
            "get" => self.handle_get(input, start).await,
            "schema" => Ok(handle_schema(start)),
            "validate" => Ok(handle_validate(input, start)),
            "save" => self.handle_save(input, start).await,
            "delete" => self.handle_delete(input, start).await,
            "start" => self.handle_start(input, start).await,
            "continue" => Ok(handle_continue(start)),
            "exit" => Ok(handle_exit(start)),
            other => Ok(ToolResult::failure(
                format!(
                    "Unknown action '{other}'. Use list, get, schema, validate, save, delete, start, continue, or exit."
                ),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

impl PlaybookTool {
    async fn handle_list(&self, start: Instant) -> ToolResult {
        // Only the playbooks this turn's user can start: global, their own, and
        // any shared with them.
        let scope =
            DefListing::for_turn(&self.write_auth, self.runtime.def_store(), KIND_PLAYBOOK).await;
        let summaries = self.runtime.list_definitions_visible(&scope).await;
        let list: Vec<serde_json::Value> = summaries
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "name": p.name,
                    "description": p.description,
                    "steps": p.step_count,
                    "owner": p.owner,
                    "visibility": p.visibility,
                })
            })
            .collect();
        ToolResult::success(
            serde_json::json!({ "playbooks": list, "count": list.len() }),
            start.elapsed().as_millis() as u64,
        )
    }

    /// The row `get` reads: the named owner's, when the turn's user may read
    /// it; otherwise the user's own copy of `id` when there is one, else the
    /// shared definition. A row the user may not read is, to them, not there.
    async fn readable_owner(
        &self,
        id: &str,
        requested: Option<&str>,
    ) -> std::result::Result<Option<String>, String> {
        let Some(owner) = requested else {
            let own = match current_turn_user() {
                Some(user) => self
                    .runtime
                    .definition_ownership(id, Some(&user))
                    .await
                    .map(|_| user),
                None => None,
            };
            return Ok(own);
        };
        let not_found = || format!("Playbook '{id}' not found");
        let (_, visibility) = self
            .runtime
            .definition_ownership(id, Some(owner))
            .await
            .ok_or_else(not_found)?;
        self.write_auth
            .may_read(
                self.runtime.def_store().cloned(),
                workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
                id,
                Some(owner),
                visibility,
            )
            .await
            .then(|| Some(owner.to_string()))
            .ok_or_else(not_found)
    }

    async fn handle_get(&self, input: PlaybookInput, start: Instant) -> Result<ToolResult> {
        let requested = requested_owner(&input);
        let Some(id) = input.playbook_id.filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'playbook_id' — which playbook to retrieve?",
                start.elapsed().as_millis() as u64,
            ));
        };
        let owner = match self.readable_owner(&id, requested.as_deref()).await {
            Ok(owner) => owner,
            Err(e) => return Ok(ToolResult::failure(e, start.elapsed().as_millis() as u64)),
        };
        match self.runtime.get_definition_source(&id, owner.as_deref()) {
            Ok(toml) => Ok(ToolResult::success(
                serde_json::json!({ "playbook_id": id, "owner": owner, "toml": toml }),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("{e:#}"),
                start.elapsed().as_millis() as u64,
            )),
        }
    }

    /// The shared rule, for the turn's user, on the row `owner` names.
    async fn authorize_write(
        &self,
        id: &str,
        owner: Option<&str>,
        op: DefOp,
    ) -> std::result::Result<DefWriteGrant, DefWriteDenied> {
        self.write_auth
            .authorize(
                self.runtime.def_store().cloned(),
                workforce_orchestrator::workflow_store::KIND_PLAYBOOK,
                id,
                owner,
                op,
            )
            .await
    }

    /// Whether a shared (system) playbook with this id exists — what a
    /// member's own copy shadows.
    async fn shadows_shared(&self, id: &str, grant: &DefWriteGrant) -> bool {
        grant.owner.is_some() && self.runtime.definition_ownership(id, None).await.is_some()
    }

    async fn handle_save(&self, input: PlaybookInput, start: Instant) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;
        let requested = requested_owner(&input);
        let Some(id) = input.playbook_id.filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'playbook_id' — what id should this playbook have?",
                elapsed(),
            ));
        };
        let Some(toml_content) = input.toml_content.filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'toml_content' — the playbook TOML to save.",
                elapsed(),
            ));
        };
        let grant = match self
            .authorize_write(&id, requested.as_deref(), DefOp::Save)
            .await
        {
            Ok(grant) => grant,
            Err(denied) => {
                return Ok(ToolResult::failure(
                    format!("Cannot save playbook '{id}': {denied}"),
                    elapsed(),
                ));
            }
        };
        let stored = self
            .runtime
            .definition_ownership(&id, grant.owner.as_deref())
            .await
            .map(|(_, visibility)| visibility);
        let visibility = match write_visibility(&grant, input.visibility.as_deref(), stored) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::failure(e, elapsed())),
        };
        match self
            .runtime
            .save_definition_source(
                &id,
                &toml_content,
                grant.owner.as_deref(),
                visibility,
                workforce_orchestrator::workflow_store::EditContext::new(
                    "agent_tool",
                    grant.actor.as_deref(),
                ),
            )
            .await
        {
            Ok(()) => {
                let message = if self.shadows_shared(&id, &grant).await {
                    format!(
                        "Saved as your {} copy of '{id}'; the shared '{id}' is unchanged.",
                        visibility.as_str()
                    )
                } else {
                    format!("Playbook '{id}' saved and loaded")
                };
                Ok(ToolResult::success(
                    serde_json::json!({
                        "saved": true,
                        "playbook_id": id,
                        "owner": grant.owner,
                        "visibility": visibility,
                        "message": message,
                    }),
                    elapsed(),
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to save playbook: {e:#}"),
                elapsed(),
            )),
        }
    }

    async fn handle_delete(&self, input: PlaybookInput, start: Instant) -> Result<ToolResult> {
        let elapsed = || start.elapsed().as_millis() as u64;
        let requested = requested_owner(&input);
        let Some(id) = input.playbook_id.filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'playbook_id' — which playbook to delete?",
                elapsed(),
            ));
        };
        let grant = match self
            .authorize_write(&id, requested.as_deref(), DefOp::Delete)
            .await
        {
            Ok(grant) => grant,
            Err(denied) => {
                return Ok(ToolResult::failure(
                    format!("Cannot delete playbook '{id}': {denied}"),
                    elapsed(),
                ));
            }
        };
        // The rule places a member on their own row; deleting a row they do
        // not have must not read as deleting the shared one.
        if self
            .runtime
            .definition_ownership(&id, grant.owner.as_deref())
            .await
            .is_none()
        {
            let message = if self.shadows_shared(&id, &grant).await {
                format!(
                    "You have no copy of '{id}' to delete; the shared '{id}' is the system's and \
                     only an administrator may delete it."
                )
            } else {
                format!("Playbook '{id}' not found")
            };
            return Ok(ToolResult::failure(message, elapsed()));
        }
        match self
            .runtime
            .delete_definition(
                &id,
                grant.owner.as_deref(),
                workforce_orchestrator::workflow_store::EditContext::new(
                    "agent_tool",
                    grant.actor.as_deref(),
                ),
            )
            .await
        {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({ "deleted": true, "playbook_id": id, "owner": grant.owner }),
                elapsed(),
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to delete playbook: {e:#}"),
                elapsed(),
            )),
        }
    }

    /// Request that a playbook start. This only validates and acknowledges —
    /// the dispatch layer watches for a successful `start` and re-injects
    /// `cmd: playbook start <id>` to begin it in a fresh turn (a tool can't set
    /// durable session state mid-turn). On an unknown id this returns a failure
    /// so the dispatch layer does NOT re-inject.
    async fn handle_start(&self, input: PlaybookInput, start: Instant) -> Result<ToolResult> {
        let Some(id) = input.playbook_id.filter(|s| !s.is_empty()) else {
            return Ok(ToolResult::failure(
                "Missing 'playbook_id' — which playbook to start?",
                start.elapsed().as_millis() as u64,
            ));
        };
        // Validate the same way the re-injected `cmd: playbook start <id>` will
        // resolve it (own playbook shadows global), so a successful ack here
        // can't be followed by a "no such playbook" at dispatch.
        let user = current_turn_user().unwrap_or_default();
        match self
            .runtime
            .resolve_for_user(&id, &user, &self.write_auth.teams())
            .await
            .found()
        {
            Some(def) => Ok(ToolResult::success(
                serde_json::json!({
                    "starting": id,
                    "name": def.playbook.name,
                    "message": format!(
                        "Starting the '{}' playbook in this chat. STOP HERE — give the user a \
                         one-sentence acknowledgement and end your turn; step 1 begins \
                         automatically in a moment. Do NOT do the playbook's work yourself now.",
                        def.playbook.name
                    ),
                }),
                start.elapsed().as_millis() as u64,
            )),
            None => Ok(ToolResult::failure(
                format!("Unknown playbook '{id}'. Use action 'list' to see available playbooks."),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Request that the active playbook advance. Like `start`, this only
/// acknowledges; the dispatch layer re-injects `cmd: continue` after the turn so
/// the advance happens in a fresh turn (a tool can't mutate session state mid-turn).
fn handle_continue(start: Instant) -> ToolResult {
    ToolResult::success(
        serde_json::json!({
            "advancing": true,
            "message": "Advancing the playbook to the next step. STOP HERE — tell the user in one \
                short sentence that you're moving on; the next step begins automatically in a moment. \
                Only do this when the user asked to proceed or the step's goal is unambiguously met.",
        }),
        start.elapsed().as_millis() as u64,
    )
}

/// Request that the active playbook exit. Mirrors `continue`: the dispatch layer
/// re-injects `cmd: exit` after the turn, clearing the playbook (chat continues).
fn handle_exit(start: Instant) -> ToolResult {
    ToolResult::success(
        serde_json::json!({
            "exiting": true,
            "message": "Exiting the playbook. Tell the user in one short sentence that you're \
                stopping the playbook (and why); normal conversation continues afterwards.",
        }),
        start.elapsed().as_millis() as u64,
    )
}

fn handle_schema(start: Instant) -> ToolResult {
    ToolResult::success(
        serde_json::json!({ "schema_reference": PLAYBOOK_SCHEMA_REFERENCE }),
        start.elapsed().as_millis() as u64,
    )
}

fn handle_validate(input: PlaybookInput, start: Instant) -> ToolResult {
    let Some(toml_content) = input.toml_content.filter(|s| !s.is_empty()) else {
        return ToolResult::success(
            serde_json::json!({ "valid": false, "error": "Missing 'toml_content' to validate." }),
            start.elapsed().as_millis() as u64,
        );
    };
    match PlaybookDefinition::from_toml(&toml_content) {
        Ok(def) => {
            let step_ids: Vec<&str> = def.steps.iter().map(|s| s.id.as_str()).collect();
            ToolResult::success(
                serde_json::json!({
                    "valid": true,
                    "playbook_id": def.playbook.id,
                    "name": def.playbook.name,
                    "step_count": def.steps.len(),
                    "step_ids": step_ids,
                }),
                start.elapsed().as_millis() as u64,
            )
        }
        Err(e) => ToolResult::success(
            serde_json::json!({ "valid": false, "error": format!("{e:#}") }),
            start.elapsed().as_millis() as u64,
        ),
    }
}

const PLAYBOOK_SCHEMA_REFERENCE: &str = r#"# Playbook TOML format

A playbook is an ordered list of step prompts injected, one at a time, into the
user's normal chat session and gated by the user. The user starts it with
`cmd: playbook start <id>`, chats with you freely at each step, then advances
with `cmd: continue` (or the Continue button) or stops with `cmd: exit`.

Unlike a workflow (fresh agent per step + variable handoff), a playbook keeps
ONE persistent session — the whole conversation stays present. You drive each
step in real time; the playbook only decides what prompt comes next.

## Structure

```toml
[playbook]
id = "design-impl-review"          # unique, kebab-case; must match the filename
name = "Design → Implement → Review"
description = "What this playbook is for (one line)."

[[step]]
id = "design"                      # unique within the playbook
title = "Design the implementation" # short label shown on the ▶ Step N divider
artifact = "design.md"             # optional: a file this step should produce
prompt = """
Multi-line instruction for this step. Ask the user what they need, do the work,
and talk it through until they Continue.
"""

[[step]]
id = "implement"
title = "Implement"
prompt = "Implement the design (read design.md). ..."
```

## Fields

- `[playbook]`: `id` (required, must equal the saved filename), `name`
  (required), `description` (optional).
- `[[step]]` (one or more, in order): `id` (required, unique), `title`
  (required, short), `prompt` (required, the instruction), `artifact`
  (optional filename).

## Artifacts — surviving compaction

If a step sets `artifact = "design.md"`, an instruction to write that file is
auto-appended to its prompt, and on Continue the file is pinned. Later steps are
told to read it, and a synopsis is kept in your system prompt every turn — so
the substance survives even after the raw discussion is compacted away. Use
artifacts for outputs a later step needs (a design doc, an impl summary). The
file lives in a per-session scratch dir under your workspace; just write to the
relative path the prompt gives you.

## Validation rules

- At least one step.
- Step `id`s unique.
- Non-empty `prompt` on every step.
- The `[playbook].id` must match the `playbook_id` you save under.

## Authoring procedure

1. Call action `schema` (this) for the format.
2. Draft the TOML.
3. Call action `validate` with `toml_content` to check it.
4. Call action `save` with `playbook_id` and `toml_content` to persist + load it.
   The user can then run `cmd: playbook start <id>`.

## Example

```toml
[playbook]
id = "triage-bug"
name = "Triage a Bug"
description = "Reproduce, diagnose, and propose a fix for a reported bug."

[[step]]
id = "reproduce"
title = "Reproduce"
artifact = "repro.md"
prompt = "Ask for the bug report. Reproduce it and write the exact repro steps."

[[step]]
id = "diagnose"
title = "Diagnose"
artifact = "diagnosis.md"
prompt = "Find the root cause (read repro.md). Explain it and where it lives."

[[step]]
id = "fix"
title = "Propose a fix"
prompt = "Propose the fix (read diagnosis.md). Discuss trade-offs with the user."
```
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::{always_admin, never_admin};
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_core::access::{Grant, GrantLevel};
    use workforce_orchestrator::workflow_store::WorkflowStore;

    const GOOD: &str =
        "[playbook]\nid=\"demo\"\nname=\"Demo\"\ndescription=\"d\"\n[[step]]\nid=\"a\"\ntitle=\"A\"\nprompt=\"do\"\n";
    const EDITED: &str =
        "[playbook]\nid=\"demo\"\nname=\"Demo edited\"\ndescription=\"d\"\n[[step]]\nid=\"a\"\ntitle=\"A\"\nprompt=\"do\"\n";

    fn input(action: &str, id: Option<&str>, toml: Option<&str>) -> PlaybookInput {
        PlaybookInput {
            action: action.into(),
            playbook_id: id.map(str::to_string),
            toml_content: toml.map(str::to_string),
            owner: None,
            visibility: None,
        }
    }

    #[test]
    fn validate_accepts_good_rejects_stepless() {
        let good = handle_validate(input("validate", None, Some(GOOD)), Instant::now());
        assert_eq!(good.output["valid"], serde_json::json!(true));
        assert_eq!(good.output["step_count"], serde_json::json!(1));

        let stepless = handle_validate(
            input("validate", None, Some("[playbook]\nid=\"p\"\nname=\"P\"\n")),
            Instant::now(),
        );
        assert_eq!(stepless.output["valid"], serde_json::json!(false));
    }

    fn fs_tool(runtime: &Arc<PlaybookRuntime>, admin: bool) -> PlaybookTool {
        let is_admin = if admin { always_admin() } else { never_admin() };
        PlaybookTool::new(Arc::clone(runtime))
            .with_write_auth(TurnDefWriteAuth::default().with_admin_policy(is_admin))
    }

    fn fs_runtime() -> (Arc<PlaybookRuntime>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pbtool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        (Arc::new(PlaybookRuntime::new(dir.clone())), dir)
    }

    async fn as_user<F: std::future::Future>(uid: &str, fut: F) -> F::Output {
        CURRENT_USER_ID.scope(uid.to_string(), fut).await
    }

    fn save(toml: &str) -> serde_json::Value {
        serde_json::json!({ "action": "save", "playbook_id": "demo", "toml_content": toml })
    }

    fn delete() -> serde_json::Value {
        serde_json::json!({ "action": "delete", "playbook_id": "demo" })
    }

    #[tokio::test]
    async fn save_then_get_and_list_round_trip() {
        let (runtime, dir) = fs_runtime();
        let tool = fs_tool(&runtime, true);

        let saved = tool
            .execute(serde_json::json!({
                "action": "save", "playbook_id": "demo", "toml_content": GOOD,
            }))
            .await
            .unwrap();
        assert!(saved.success, "{saved:?}");

        let got = tool
            .execute(serde_json::json!({ "action": "get", "playbook_id": "demo" }))
            .await
            .unwrap();
        assert!(got.output["toml"].as_str().unwrap().contains("Demo"));

        let listed = tool
            .execute(serde_json::json!({ "action": "list" }))
            .await
            .unwrap();
        assert_eq!(listed.output["count"], serde_json::json!(1));

        // id ≠ TOML id is rejected.
        let mismatch = tool
            .execute(serde_json::json!({
                "action": "save", "playbook_id": "other", "toml_content": GOOD,
            }))
            .await
            .unwrap();
        assert!(!mismatch.success);

        // start succeeds only for a known id (success gates the re-injection).
        let ok = tool
            .execute(serde_json::json!({ "action": "start", "playbook_id": "demo" }))
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(ok.output["starting"], serde_json::json!("demo"));

        let unknown = tool
            .execute(serde_json::json!({ "action": "start", "playbook_id": "nope" }))
            .await
            .unwrap();
        assert!(!unknown.success);

        // continue/exit need no playbook_id and always acknowledge (success
        // gates the dispatch-layer re-injection of `cmd: continue` / `cmd: exit`).
        let cont = tool
            .execute(serde_json::json!({ "action": "continue" }))
            .await
            .unwrap();
        assert!(cont.success);
        assert_eq!(cont.output["advancing"], serde_json::json!(true));

        let exit = tool
            .execute(serde_json::json!({ "action": "exit" }))
            .await
            .unwrap();
        assert!(exit.success);
        assert_eq!(exit.output["exiting"], serde_json::json!(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A member's save and delete address their own row, never the shared
    /// system playbook — on a filesystem deployment there is no such row, so
    /// the shared one is left exactly as it was.
    #[tokio::test]
    async fn member_cannot_save_over_or_delete_the_shared_playbook() {
        let (runtime, dir) = fs_runtime();
        assert!(
            fs_tool(&runtime, true)
                .execute(save(GOOD))
                .await
                .unwrap()
                .success
        );
        let member = fs_tool(&runtime, false);

        let saved = as_user("alice", member.execute(save(EDITED)))
            .await
            .unwrap();
        assert!(!saved.success, "{saved:?}");
        assert!(saved
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("database store"));
        assert_eq!(
            std::fs::read_to_string(dir.join("demo.toml")).unwrap(),
            GOOD
        );

        let deleted = as_user("alice", member.execute(delete())).await.unwrap();
        assert!(!deleted.success, "{deleted:?}");
        assert!(
            deleted
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("only an administrator may delete it"),
            "{deleted:?}"
        );
        assert!(dir.join("demo.toml").exists());

        let admin_deleted = fs_tool(&runtime, true).execute(delete()).await.unwrap();
        assert!(admin_deleted.success, "{admin_deleted:?}");
        assert!(!dir.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unbound_turn_cannot_save_or_delete() {
        let (runtime, dir) = fs_runtime();
        assert!(
            fs_tool(&runtime, true)
                .execute(save(GOOD))
                .await
                .unwrap()
                .success
        );
        let nobody = fs_tool(&runtime, false);
        for input in [save(EDITED), delete()] {
            let res = nobody.execute(input).await.unwrap();
            assert!(!res.success, "{res:?}");
            assert!(res
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("not signed in"));
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("demo.toml")).unwrap(),
            GOOD
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Naming another member's row is an administrator's override, so what
    /// refuses here is the deployment rather than the rule: a filesystem hub
    /// holds no member's row at all. A member is still refused by the rule,
    /// and neither touches the shared playbook.
    #[tokio::test]
    async fn naming_another_members_row_reaches_it_rather_than_the_shared_one() {
        let (runtime, dir) = fs_runtime();
        let named = |mut input: serde_json::Value| {
            input["owner"] = serde_json::json!("bob");
            input
        };
        let admin = fs_tool(&runtime, true);
        for (input, refusal) in [
            (save(GOOD), "require a database store"),
            (delete(), "not found"),
        ] {
            let res = as_user("root", admin.execute(named(input))).await.unwrap();
            assert!(!res.success, "{res:?}");
            assert!(
                res.error.as_deref().unwrap_or_default().contains(refusal),
                "{res:?}"
            );
        }

        let member = fs_tool(&runtime, false);
        for input in [save(GOOD), delete()] {
            let res = as_user("alice", member.execute(named(input)))
                .await
                .unwrap();
            assert!(!res.success, "{res:?}");
            assert!(
                res.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("owned by bob"),
                "{res:?}"
            );
        }
        assert!(!dir.join("demo.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a store, a member's save is their own copy and their delete removes
    /// only that copy; the shared playbook needs an administrator.
    #[tokio::test(flavor = "multi_thread")]
    async fn member_copy_is_saved_and_deleted_apart_from_the_shared_playbook() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let (runtime, dir) = fs_runtime();
                runtime.set_store(Arc::clone(&store));
                let admin = fs_tool(&runtime, true);
                let member = fs_tool(&runtime, false);
                assert!(admin.execute(save(GOOD)).await.unwrap().success);

                let saved = as_user("alice", member.execute(save(EDITED)))
                    .await
                    .unwrap();
                assert!(saved.success, "{saved:?}");
                assert_eq!(saved.output["owner"], "alice");
                assert_eq!(saved.output["visibility"], "private");
                assert!(
                    saved.output["message"]
                        .as_str()
                        .unwrap()
                        .contains("the shared 'demo' is unchanged"),
                    "{saved:?}"
                );
                assert_eq!(
                    store
                        .get("demo", "playbook", None)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(GOOD)
                );
                let got = as_user(
                    "alice",
                    member.execute(serde_json::json!({ "action": "get", "playbook_id": "demo" })),
                )
                .await
                .unwrap();
                assert_eq!(got.output["owner"], "alice");
                assert!(got.output["toml"].as_str().unwrap().contains("Demo edited"));

                // A run-share grantee reads alice's copy by naming her; others
                // see nothing there.
                store
                    .set_shares(
                        "playbook",
                        "demo",
                        "alice",
                        &[Grant::user("bob", GrantLevel::Run)],
                        "alice",
                    )
                    .await
                    .unwrap();
                let get_alices = || {
                    serde_json::json!({ "action": "get", "playbook_id": "demo", "owner": "alice" })
                };
                let bob = as_user("bob", member.execute(get_alices())).await.unwrap();
                assert!(bob.success, "{bob:?}");
                assert!(bob.output["toml"].as_str().unwrap().contains("Demo edited"));
                let carol = as_user("carol", member.execute(get_alices())).await.unwrap();
                assert!(!carol.success, "{carol:?}");
                assert!(carol.error.as_deref().unwrap_or_default().contains("not found"));

                let deleted = as_user("alice", member.execute(delete())).await.unwrap();
                assert!(deleted.success, "{deleted:?}");
                assert_eq!(deleted.output["owner"], "alice");
                assert!(store
                    .get("demo", "playbook", Some("alice"))
                    .await
                    .unwrap()
                    .is_none());
                assert!(store.get("demo", "playbook", None).await.unwrap().is_some());

                let again = as_user("alice", member.execute(delete())).await.unwrap();
                assert!(!again.success, "{again:?}");
                assert!(again
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no copy of 'demo'"));

                let admin_deleted = as_user("root", admin.execute(delete())).await.unwrap();
                assert!(admin_deleted.success, "{admin_deleted:?}");
                assert!(store.get("demo", "playbook", None).await.unwrap().is_none());
                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }
}
