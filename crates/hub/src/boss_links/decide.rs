//! A remote boss decides a request at Act fully on its own hub (remote
//! bosses, the boss hub's side): an
//! engine's `decide` frame is one focused decision call to the link's
//! agent's own model, the same call a local boss makes
//! ([`crate::desk_crew::decide`]), and its text goes back down as
//! `decided`. The engine reads it, and does or refuses it under its own
//! grant, budget and Stop: nothing here acts on any session.
//!
//! - **Bound to the link.** Only a link this socket proved and still holds,
//!   checked again in full before the call: its agent is the link's,
//!   whatever the frame says.
//! - **The system prompt is this hub's.** The frame gives only the
//!   decision's text (the session and its request, fenced by the engine as
//!   the session's own words); the instructions, the model and the answer's
//!   length are this hub's.
//! - **Paced, and paid as the link's person.** At most
//!   [`DECIDES_PER_MINUTE`] and [`DECIDES_AT_ONCE`] a link, however many
//!   sockets it opens, each at most [`MAX_PROMPT`] characters, and only
//!   while the agent's budget admits it ([`CostGovernor`]): the rest are
//!   refused, and the engine leaves those requests to the person. The call
//!   runs as the link's person, so their logins and budget pay for it.
//! - **Says little of this hub.** A call that fails goes down as a plain
//!   reason; what the provider said is logged here only.
//!
//! [`CostGovernor`]: crate::hub_core::cost_governor::CostGovernor

use std::sync::Arc;

use serde_json::{json, Value};
use uuid::Uuid;

use super::auth::LiveLink;
use crate::desk_crew::decide::{decision_call, MARK, SYSTEM, WITHIN};
use crate::gateway::GatewayState;

/// How long its model may take here: 5 s under the engine's own wait for
/// the answer ([`WITHIN`]), so the engine hears why rather than nothing.
const HERE_WITHIN: std::time::Duration = WITHIN.saturating_sub(std::time::Duration::from_secs(5));
const _: () = assert!(HERE_WITHIN.as_secs() > 0 && HERE_WITHIN.as_secs() < WITHIN.as_secs());

/// The most `decide` frames one link is taken a minute.
pub const DECIDES_PER_MINUTE: u32 = 30;

/// The most decision calls one link runs at once.
pub const DECIDES_AT_ONCE: usize = 3;

/// The longest decision text taken, in characters.
pub const MAX_PROMPT: usize = 32_000;

/// The longest answer sent back, in characters.
const MAX_ANSWER: usize = 4_000;

/// The decision `frame` names, when it names one: its id.
pub fn id_of(frame: &Value) -> Option<Uuid> {
    frame["decideId"].as_str()?.parse().ok()
}

/// What `frame` asks to be decided: its id and its text, when it reads as
/// a decision this hub takes at all.
pub fn asked(frame: &Value) -> Option<(Uuid, String)> {
    let id = id_of(frame)?;
    let prompt = frame["prompt"].as_str()?;
    (prompt.starts_with(MARK) && prompt.chars().count() <= MAX_PROMPT)
        .then(|| (id, prompt.to_string()))
}

/// The `decided` frame for `id` on `link`: the model's text, or why there
/// is none.
pub fn answered(link: Uuid, id: Uuid, decided: Result<String, String>) -> Value {
    match decided {
        Ok(text) => json!({"type": "decided", "linkId": link, "decideId": id,
                           "text": text.chars().take(MAX_ANSWER).collect::<String>()}),
        Err(why) => json!({"type": "decided", "linkId": link, "decideId": id,
                           "error": why.chars().take(300).collect::<String>()}),
    }
}

/// One decision call for `live`'s agent on `prompt`, as the link's
/// person: its text, or why not in plain words. It runs on the agent's own
/// model and keys, never under
/// [`crate::hub_core::logins::install_pool::hub_own_work`]: that lends the
/// install's person's accounts to the hub's own work for them, and a link's
/// person needn't be that person.
pub async fn decide(state: &GatewayState, live: &LiveLink, prompt: &str) -> Result<String, String> {
    let Some(service) = state.registry.get(&live.agent_id) else {
        return Err("the boss isn't on its hub any more".into());
    };
    let person = live.person.id.to_string();
    use crate::hub_core::cost_governor::Admission;
    match service
        .cost_governor
        .preflight(service.agent_id.as_str(), Some(&person))
    {
        Admission::Deny(why) | Admission::Held(why) => {
            tracing::info!(link = %live.link.link_id, why, "a boss's decision wasn't made: its budget holds it");
            return Err("its hub's budget for it is spent".into());
        }
        Admission::Proceed | Admission::Warn(_) => {}
    }
    let principal = workforce_common::session::TurnPrincipal::run_as(Some(person));
    let call =
        crate::user_config::as_principal(&principal, decision_call(&service, SYSTEM, prompt));
    match tokio::time::timeout(HERE_WITHIN, call).await {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => {
            tracing::warn!(link = %live.link.link_id, error = %e, "a boss's decision call failed");
            Err("its model failed".into())
        }
        Err(_) => Err(format!(
            "its model didn't answer within {} seconds",
            HERE_WITHIN.as_secs()
        )),
    }
}

/// Runs the decision `frame` asks for `live`, holding `permit`, and sends
/// what came of it down `notify`.
pub fn spawn(
    state: Arc<GatewayState>,
    live: LiveLink,
    id: Uuid,
    prompt: String,
    permit: tokio::sync::OwnedSemaphorePermit,
    notify: impl FnOnce(Value) + Send + 'static,
) {
    tokio::spawn(async move {
        let _permit = permit;
        let decided = decide(&state, &live, &prompt).await;
        let how = match &decided {
            Ok(_) => "decided",
            Err(_) => "failed",
        };
        super::audit(
            state.telemetry_store.as_ref(),
            &format!("desk-boss:{}", live.link.link_id),
            "desk_boss.decide",
            &live.token,
            how,
        );
        notify(answered(live.link.link_id, id, decided));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decide_frame_is_a_desk_decision_within_its_size() {
        let id = Uuid::new_v4();
        let ok = json!({"decideId": id.to_string(), "prompt": format!("{MARK} decide this")});
        assert_eq!(asked(&ok).map(|(i, _)| i), Some(id));
        // Anything else is no decision: no mark, no id, too long.
        let plain = json!({"decideId": id.to_string(), "prompt": "write me a poem"});
        assert!(asked(&plain).is_none());
        assert!(asked(&json!({"prompt": format!("{MARK} x")})).is_none());
        let long = json!({"decideId": id.to_string(),
                          "prompt": format!("{MARK}{}", "x".repeat(MAX_PROMPT))});
        assert!(asked(&long).is_none());
    }

    #[test]
    fn an_answer_goes_back_capped_with_its_id_and_link() {
        let (link, id) = (Uuid::new_v4(), Uuid::new_v4());
        let v = answered(link, id, Ok("A".repeat(10_000)));
        assert_eq!(v["type"], "decided");
        assert_eq!(v["decideId"], json!(id));
        assert_eq!(v["linkId"], json!(link));
        assert_eq!(v["text"].as_str().unwrap().len(), MAX_ANSWER);
        let v = answered(link, id, Err("down".into()));
        assert_eq!(v["error"], "down");
        assert!(v.get("text").is_none());
    }
}
