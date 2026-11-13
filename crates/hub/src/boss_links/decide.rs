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
//! - **Paced.** At most [`DECIDES_PER_MINUTE`] a link, [`DECIDES_AT_ONCE`]
//!   a socket, each at most [`MAX_PROMPT`] characters: the rest are refused,
//!   and the engine leaves those requests to the person.

use std::sync::Arc;

use serde_json::{json, Value};
use uuid::Uuid;

use super::auth::LiveLink;
use crate::desk_crew::decide::{decision_call, MARK, SYSTEM, WITHIN};

/// How long its model may take here: under the engine's own wait for the
/// answer ([`WITHIN`]), so it hears why rather than nothing.
const HERE_WITHIN: std::time::Duration = WITHIN.saturating_sub(std::time::Duration::from_secs(5));
use crate::gateway::GatewayState;

/// The most `decide` frames one link is taken a minute.
pub const DECIDES_PER_MINUTE: u32 = 30;

/// The most decision calls one socket runs at once.
pub const DECIDES_AT_ONCE: usize = 3;

/// The longest decision text taken, in characters.
pub const MAX_PROMPT: usize = 64_000;

/// The longest answer sent back, in characters.
const MAX_ANSWER: usize = 4_000;

/// What `frame` asks to be decided: its id and its text, when it reads as
/// a decision at all.
pub fn asked(frame: &Value) -> Option<(Uuid, String)> {
    let id: Uuid = frame["decideId"].as_str()?.parse().ok()?;
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

/// One decision call for `live`'s agent on `prompt`: its text, or why not.
/// It runs on the agent's own model and keys, never under
/// [`crate::hub_core::logins::install_pool::hub_own_work`]: that lends the
/// install's person's accounts to the hub's own work for them, and a link's
/// person needn't be that person.
pub async fn decide(state: &GatewayState, live: &LiveLink, prompt: &str) -> Result<String, String> {
    let Some(service) = state.registry.get(&live.agent_id) else {
        return Err("The boss isn't on its hub any more.".into());
    };
    match tokio::time::timeout(HERE_WITHIN, decision_call(&service, SYSTEM, prompt)).await {
        Ok(r) => r,
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
