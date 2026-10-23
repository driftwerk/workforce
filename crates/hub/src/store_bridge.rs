//! The document store behind `wf_store_call`: a WASM skill's requests run
//! through the same [`crate::store_tool::perform`] as the platform tools,
//! as the calling agent (and the calling user, when the run has one).
//!
//! A skill is never an admin here — `global` is read-only to it and another
//! agent's scope is out of reach — and the manifest's `store` capability is
//! the consent surface: it is part of the contract fingerprint, so a skill
//! that starts using the store needs a fresh grant.

use std::sync::Arc;

use serde_json::{json, Value};
use workforce_wasm_runtime::types::{SkillCallContext, SkillDocumentStore};

use crate::store::{Caller, DocumentStore, StoreError};
use crate::store_tool::{perform, StoreOp};

pub struct HubSkillDocumentStore {
    store: Arc<dyn DocumentStore>,
    handle: tokio::runtime::Handle,
}

impl HubSkillDocumentStore {
    pub fn new(store: Arc<dyn DocumentStore>, handle: tokio::runtime::Handle) -> Self {
        Self { store, handle }
    }

    /// Install as the process-wide bridge every WASM execution sees.
    pub fn install(store: Arc<dyn DocumentStore>, handle: tokio::runtime::Handle) {
        workforce_wasm_runtime::types::set_skill_document_store(Arc::new(Self::new(store, handle)));
    }
}

/// The skill's caller: the run's agent and user, never admin.
pub fn caller_from_skill(ctx: &SkillCallContext) -> Caller {
    Caller {
        identity: ctx.user_id.clone(),
        agent_id: Some(ctx.agent_id.clone()),
        admin: false,
        system: ctx.user_id.as_deref() == Some("system"),
    }
}

/// A store error as the guest receives it, inside the reply.
pub fn failure_reply(e: &StoreError) -> Value {
    let mut reply = json!({"ok": false, "code": e.code(), "error": e.to_string()});
    match e {
        StoreError::VersionConflict { expected, actual } => {
            reply["expected"] = json!(expected);
            reply["actual"] = json!(actual);
        }
        StoreError::Claimed { owner, until } => {
            reply["owner"] = json!(owner);
            reply["until"] = json!(until);
        }
        _ => {}
    }
    reply
}

impl SkillDocumentStore for HubSkillDocumentStore {
    fn call(&self, ctx: &SkillCallContext, request: &Value) -> Result<Value, String> {
        let op_name = request.get("op").and_then(Value::as_str).unwrap_or("");
        let Some(op) = StoreOp::parse(op_name) else {
            return Ok(failure_reply(&StoreError::Invalid(format!(
                "unknown store op `{op_name}` — expected get, set, update, delete, list, query, batch, claim or release"
            ))));
        };
        let caller = caller_from_skill(ctx);
        let store = Arc::clone(&self.store);
        let outcome = self
            .handle
            .block_on(async move { perform(store.as_ref(), &caller, op, request).await });
        match outcome {
            Ok(mut reply) => {
                if let Some(m) = reply.as_object_mut() {
                    m.insert("ok".into(), Value::Bool(true));
                }
                Ok(reply)
            }
            // A denial is the one outcome the host turns into a notice, so
            // the agent reading the skill's failure sees the remedy.
            Err(StoreError::Denied(why)) => Err(format!(
                "Skill '{}' may not reach that store scope: {why}",
                ctx.skill_name
            )),
            Err(e) => Ok(failure_reply(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryDocumentStore;

    fn ctx(user: Option<&str>) -> SkillCallContext {
        SkillCallContext {
            skill_name: "ticket-picker".into(),
            grant_fingerprint: "fp".into(),
            agent_id: "atlas".into(),
            user_id: user.map(str::to_string),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_skill_writes_its_agents_scope_and_reads_failures_in_band() {
        let store: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::new());
        let bridge = HubSkillDocumentStore::new(store, tokio::runtime::Handle::current());
        let c = ctx(Some("alice"));
        let reply = tokio::task::spawn_blocking({
            let bridge = Arc::new(bridge);
            let c = c.clone();
            move || {
                let set = bridge
                    .call(&c, &json!({"op": "set", "collection": "pipeline", "doc_id": "ENG-1", "body": {"stage": "pick"}}))
                    .unwrap();
                assert_eq!(set["ok"], true);
                assert_eq!(set["version"], 1);
                let stale = bridge
                    .call(&c, &json!({"op": "set", "collection": "pipeline", "doc_id": "ENG-1", "body": {}, "if_version": 9}))
                    .unwrap();
                assert_eq!(stale["ok"], false);
                assert_eq!(stale["code"], "version_conflict");
                assert_eq!(stale["actual"], 1);
                let bad = bridge.call(&c, &json!({"op": "explode"})).unwrap();
                assert_eq!(bad["code"], "invalid");
                // global is read-only to a skill: a denial is a notice, not a reply
                let denied = bridge.call(&c, &json!({"op": "set", "scope": "global", "collection": "x", "doc_id": "1", "body": {}}));
                assert!(denied.unwrap_err().contains("ticket-picker"));
                // and the user scope needs a user
                let no_user = bridge
                    .call(&ctx(None), &json!({"op": "get", "scope": "user", "collection": "x", "doc_id": "1"}))
                    .unwrap();
                assert_eq!(no_user["code"], "no_identity");
                bridge
                    .call(&c, &json!({"op": "get", "collection": "pipeline", "doc_id": "ENG-1"}))
                    .unwrap()
            }
        })
        .await
        .unwrap();
        assert_eq!(reply["found"], true);
        assert_eq!(reply["document"]["body"]["stage"], "pick");
    }
}
