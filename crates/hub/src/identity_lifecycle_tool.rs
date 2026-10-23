//! `identity.lifecycle` — the admin tool face of the identity lifecycle:
//! what an identity holds, suspending and reactivating it, merging it into
//! another, transferring one item,
//! deleting it, the operations recorded, and what identities that no longer
//! exist left behind. Only an active System Admin may
//! use it, merge and delete default to a dry run, and every call waits for a
//! person to approve it, so no scheduled job, workflow step or other
//! unattended run can ever merge, transfer or delete anyone. A real merge,
//! delete or clean-up starts in the background once approved and answers
//! with its operation; `operation` reads its progress and report.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use workforce_agent::tools::{Tool, ToolResult};

use crate::identity_lifecycle::{
    Actor, DeletePlan, Item, Launched, Lifecycle, LifecycleError, Request,
};

pub struct IdentityLifecycleTool;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    Holdings {
        identity: String,
    },
    Suspend {
        identity: String,
    },
    Resume {
        identity: String,
    },
    Merge {
        identity: String,
        into: String,
        #[serde(default = "dry_run_default")]
        dry_run: bool,
    },
    Transfer {
        identity: String,
        item: Item,
        to: String,
    },
    Delete {
        identity: String,
        #[serde(default)]
        recipient: Option<String>,
        #[serde(default)]
        choices: std::collections::BTreeMap<
            crate::identity_lifecycle::Category,
            crate::identity_lifecycle::Choice,
        >,
        #[serde(default = "dry_run_default")]
        dry_run: bool,
    },
    Operations {
        #[serde(default)]
        identity: Option<String>,
    },
    Operation {
        id: Uuid,
    },
    Orphans,
    TransferOrphan {
        identity: Uuid,
        to: String,
        #[serde(default)]
        move_personal: bool,
        #[serde(default = "dry_run_default")]
        dry_run: bool,
    },
    DeleteOrphan {
        identity: Uuid,
        #[serde(default)]
        recipient: Option<String>,
        #[serde(default)]
        choices: std::collections::BTreeMap<
            crate::identity_lifecycle::Category,
            crate::identity_lifecycle::Choice,
        >,
        #[serde(default = "dry_run_default")]
        dry_run: bool,
    },
}

fn dry_run_default() -> bool {
    true
}

/// An identity by id or email.
fn resolve(lifecycle: &Lifecycle, named: &str) -> Result<Uuid, String> {
    if let Ok(id) = Uuid::parse_str(named) {
        return Ok(id);
    }
    lifecycle
        .identities
        .get_by_email(named)
        .map_err(|e| e.to_string())?
        .map(|identity| identity.id)
        .ok_or_else(|| format!("No identity has the id or email {named}"))
}

/// The caller, when they are an active System Admin.
fn admin(lifecycle: &Lifecycle) -> Result<Actor, String> {
    let caller = crate::user_config::current_identity_id()
        .and_then(|id| Uuid::parse_str(&id).ok())
        .ok_or("identity.lifecycle needs a signed-in System Admin")?;
    let identity = lifecycle
        .identities
        .get_by_id(caller)
        .map_err(|e| e.to_string())?
        .ok_or("identity.lifecycle needs a signed-in System Admin")?;
    match crate::authz::is_effective_admin(identity.role, identity.status) {
        true => Ok(Actor(Some(caller))),
        false => Err("Only an active System Admin may use identity.lifecycle".into()),
    }
}

fn json(value: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// A dry run's report, or the operation a real run started.
async fn launch(
    lifecycle: &Arc<Lifecycle>,
    actor: &Actor,
    request: Request,
    dry_run: bool,
) -> Result<Value, String> {
    match lifecycle.launch(request, actor.clone(), dry_run).await {
        Ok(Launched::Report(report)) => json(report),
        Ok(Launched::Started(op)) => Ok(json!({
            "operation": op,
            "note": "Started in the background. Read its progress and report with action operation and this id."
        })),
        Err(e) => Err(format!("{e:#}")),
    }
}

async fn run(lifecycle: &Arc<Lifecycle>, actor: &Actor, action: Action) -> Result<Value, String> {
    let failed = |e: LifecycleError| format!("{e:#}");
    let plan = |recipient: Option<String>, choices| -> Result<DeletePlan, String> {
        Ok(DeletePlan {
            recipient: recipient
                .map(|named| resolve(lifecycle, &named))
                .transpose()?,
            choices,
        })
    };
    match action {
        Action::Holdings { identity } => {
            let id = resolve(lifecycle, &identity)?;
            json(lifecycle.holdings(id, actor).await.map_err(failed)?)
        }
        Action::Suspend { identity } => {
            let id = resolve(lifecycle, &identity)?;
            json(lifecycle.suspend(id, actor).await.map_err(failed)?)
        }
        Action::Resume { identity } => {
            let id = resolve(lifecycle, &identity)?;
            json(lifecycle.resume(id, actor).await.map_err(failed)?)
        }
        Action::Merge {
            identity,
            into,
            dry_run,
        } => {
            let (from, into) = (resolve(lifecycle, &identity)?, resolve(lifecycle, &into)?);
            launch(lifecycle, actor, Request::Merge { from, into }, dry_run).await
        }
        Action::Transfer { identity, item, to } => {
            let (from, to) = (resolve(lifecycle, &identity)?, resolve(lifecycle, &to)?);
            json(
                lifecycle
                    .transfer(from, &item, to, actor)
                    .await
                    .map_err(failed)?,
            )
        }
        Action::Delete {
            identity,
            recipient,
            choices,
            dry_run,
        } => {
            let subject = resolve(lifecycle, &identity)?;
            let plan = plan(recipient, choices)?;
            launch(lifecycle, actor, Request::Delete { subject, plan }, dry_run).await
        }
        Action::Orphans => json(lifecycle.orphans(actor).await.map_err(failed)?),
        Action::TransferOrphan {
            identity,
            to,
            move_personal,
            dry_run,
        } => {
            let request = Request::TransferOrphan {
                id: identity,
                to: resolve(lifecycle, &to)?,
                personal: move_personal,
            };
            launch(lifecycle, actor, request, dry_run).await
        }
        Action::DeleteOrphan {
            identity,
            recipient,
            choices,
            dry_run,
        } => {
            let request = Request::DeleteOrphan {
                id: identity,
                plan: plan(recipient, choices)?,
            };
            launch(lifecycle, actor, request, dry_run).await
        }
        Action::Operations { identity } => {
            let id = identity
                .map(|named| resolve(lifecycle, &named))
                .transpose()?;
            lifecycle
                .operations(id, 50)
                .map(|ops| json!({ "operations": ops }))
                .map_err(|e| format!("{e:#}"))
        }
        Action::Operation { id } => lifecycle
            .operation(id)
            .map(|op| json!({ "operation": op }))
            .map_err(failed),
    }
}

#[async_trait]
impl Tool for IdentityLifecycleTool {
    fn name(&self) -> &str {
        "identity.lifecycle"
    }

    fn description(&self) -> &str {
        "Admin only. What an identity holds across every store (holdings); suspend it, \
         holding its scheduled jobs, workflow runs and schedules and stopping its running \
         turns and sub-agent jobs (suspend), or make a suspended identity active again, \
         releasing exactly what its suspension held (resume); merge it into \
         another identity, moving everything it owns and retiring it (merge); give one \
         scheduled job, workflow, playbook, monitor or consolidation template it owns to \
         another identity (transfer); delete it with everything it holds (delete); the \
         operations recorded (operations); what identities that no longer exist left \
         behind, as counts per missing identity and category (orphans); and hand one \
         missing identity's jobs, workflows, shared artifacts and skills to a live identity, \
         erasing its credentials, devices and access and keeping its conversations, \
         memories, notes and documents unless move_personal is true (transfer_orphan), or \
         erase what it left (delete_orphan). Merge, delete, transfer_orphan and delete_orphan are dry runs \
         unless dry_run is false: show the dry run to the admin and run it for real only \
         when they confirm. A real one starts in the background and answers with its \
         operation id; read its progress and final report with operation."
    }

    fn approval_requirement(&self) -> Option<String> {
        Some(
            "Suspending, merging, transferring from and deleting identities acts on other people's data, \
             so a person approves every call"
                .to_string(),
        )
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("?");
        let real = input.get("dry_run").and_then(Value::as_bool) == Some(false);
        let mode = match real {
            true => "for real",
            false => "as a dry run",
        };
        Some(match text("action") {
            "merge" => format!("merge {} into {} {mode}", text("identity"), text("into")),
            "delete" => format!("delete {} {mode}", text("identity")),
            "orphans" => "list what identities that no longer exist left behind".to_string(),
            "transfer_orphan" => format!(
                "hand what the missing identity {} left to {}{} {mode}",
                text("identity"),
                text("to"),
                match input.get("move_personal").and_then(Value::as_bool) {
                    Some(true) => ", moving its conversations, memories, notes and documents too",
                    _ =>
                        ", keeping its conversations, memories, notes and documents where they are",
                }
            ),
            "delete_orphan" => format!(
                "erase what the missing identity {} left {mode}",
                text("identity")
            ),
            "transfer" => format!("transfer an item of {} to {}", text("identity"), text("to")),
            "holdings" => format!("list what {} holds", text("identity")),
            "suspend" => format!("suspend {}", text("identity")),
            "resume" => format!("make {} active again", text("identity")),
            "operation" => format!("read lifecycle operation {}", text("id")),
            _ => "list identity lifecycle operations".to_string(),
        })
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["holdings", "suspend", "resume", "merge", "transfer", "delete", "operations", "operation", "orphans", "transfer_orphan", "delete_orphan"]},
                "id": {"type": "string", "description": "operation: the operation id a real merge, delete, transfer_orphan or delete_orphan answered with."},
                "identity": {"type": "string", "description": "Identity id or email; for transfer_orphan and delete_orphan, the missing identity's id as orphans lists it."},
                "into": {"type": "string", "description": "merge: the identity to merge into (id or email)."},
                "to": {"type": "string", "description": "transfer, transfer_orphan: the recipient (id or email)."},
                "recipient": {"type": "string", "description": "delete, delete_orphan: who receives what is transferred rather than erased."},
                "item": {
                    "type": "object",
                    "description": "transfer: {\"kind\": \"job\", \"agent\": ..., \"id\": ...} or {\"kind\": \"workflow\" | \"playbook\" | \"monitor\" | \"consolidation_template\", \"id\": ...}."
                },
                "choices": {
                    "type": "object",
                    "description": "delete, delete_orphan: per category, \"erase\" or \"transfer\" (scheduled_jobs, definitions, artifacts).",
                    "additionalProperties": {"type": "string", "enum": ["erase", "transfer"]}
                },
                "move_personal": {"type": "boolean", "default": false, "description": "transfer_orphan: also move the missing identity's conversations, memories, notes and documents to the recipient. Only when the admin confirms the recipient is the same person."},
                "dry_run": {"type": "boolean", "default": true}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let Some(lifecycle) = crate::identity_lifecycle::installed() else {
            return Ok(ToolResult::failure(
                "Identity service not configured on this hub.",
                elapsed(),
            ));
        };
        let actor = match admin(&lifecycle) {
            Ok(actor) => actor,
            Err(why) => return Ok(ToolResult::failure(why, elapsed())),
        };
        let action: Action = match serde_json::from_value(input) {
            Ok(action) => action,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid input: {e}"),
                    elapsed(),
                ))
            }
        };
        Ok(match run(&lifecycle, &actor, action).await {
            Ok(value) => ToolResult::success(value, elapsed()),
            Err(why) => ToolResult::failure(why, elapsed()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use workforce_agent::tools::registry::ToolRegistry;

    use super::*;

    /// A job runner or a workflow tool step has nobody to approve a call, so
    /// it never reaches the tool, dry run or not.
    #[test]
    fn no_unattended_run_can_call_it() {
        let registry = ToolRegistry::new();
        registry.register(Arc::new(IdentityLifecycleTool));
        let refusal =
            workforce_agent::tools::dispatch::unattended_refusal(&registry, "identity.lifecycle");
        assert!(
            refusal.is_some_and(|why| why.contains("identity.lifecycle")),
            "an unattended path must refuse it"
        );
    }

    #[test]
    fn the_approval_prompt_says_whether_it_is_for_real() {
        let tool = IdentityLifecycleTool;
        assert_eq!(
            tool.approval_summary(
                &json!({"action": "delete", "identity": "a@x", "dry_run": false})
            )
            .as_deref(),
            Some("delete a@x for real")
        );
        assert_eq!(
            tool.approval_summary(&json!({"action": "merge", "identity": "a@x", "into": "b@x"}))
                .as_deref(),
            Some("merge a@x into b@x as a dry run")
        );
        assert_eq!(
            tool.approval_summary(&json!({
                "action": "delete_orphan",
                "identity": "4f6c",
                "dry_run": false
            }))
            .as_deref(),
            Some("erase what the missing identity 4f6c left for real")
        );
        assert_eq!(
            tool.approval_summary(&json!({"action": "operation", "id": "op-1"}))
                .as_deref(),
            Some("read lifecycle operation op-1")
        );
    }
}
