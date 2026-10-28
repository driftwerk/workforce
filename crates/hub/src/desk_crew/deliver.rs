//! A turn in an agent's Desk DM that the person didn't type: the overseer's
//! wake-ups and summaries, and the messages it (or you, through a
//! suggestion) sends the crew. It runs in the DM's own session, so it is in
//! the conversation's history, and goes out live to a DM the person has
//! open, as a typed message's answer does.

use std::sync::Arc;

use eyre::{bail, Result};
use workforce_agent::tools::approval::SelfServe;
use workforce_common::channels::DESK_CHANNEL;
use workforce_desk::crew::MentionedApproval;

use super::CrewHub;
use crate::broadcast::BroadcastMessage;
use crate::live_turns::Ending;
use crate::serve::AgentService;

impl CrewHub {
    /// Runs `prompt` as a turn in `service`'s Desk DM with `owner`, at `hop`
    /// hand-offs; `listed` are the requests a wake-up names. The session it
    /// ran in and the answer.
    pub(crate) async fn run_dm_turn(
        &self,
        service: &Arc<AgentService>,
        owner: &str,
        prompt: &str,
        hop: i16,
        listed: Option<Vec<MentionedApproval>>,
    ) -> Result<(String, String)> {
        let agent_id = service.agent_id.as_str();
        let session = crate::gateway::anchor_session(service, DESK_CHANNEL, owner, agent_id)?;
        let sid = session.id.clone();
        if hop > 0 {
            self.hops.lock().insert(sid.clone(), hop);
        }
        if let Some(listed) = listed {
            self.listed.lock().insert(sid.clone(), listed);
        }
        let tx = self.state.broadcast_tx.clone();
        let (mut live, inbox) = match service
            .begin_live_turn(&sid, SelfServe::answerable_if(false))
            .await
        {
            Ok(begun) => begun,
            Err(busy) => {
                self.hops.lock().remove(&sid);
                bail!("{busy}");
            }
        };
        let _ = tx.send(BroadcastMessage::new(sid.clone(), "typing", String::new()));
        let progress = service
            .process_message_with_progress(
                DESK_CHANNEL,
                owner,
                None,
                prompt,
                None,
                Some(&sid),
                None,
                None,
                None,
                None,
                Some(inbox),
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
                Some(live.cancel_token()),
                None,
            )
            .await?;
        let session_id = progress.session_id.clone();
        let forward = {
            let (tx, sid) = (tx.clone(), session_id.clone());
            let mentions = crate::gateway::mentions_for(service, &sid);
            move |rx| crate::gateway::forward_local_progress(rx, tx, sid, false, mentions)
        };
        let conducted = crate::live_turns::conduct(&mut live, service, progress, forward).await;
        let answer = match &conducted.ending {
            Ending::Finished(Ok(result)) => Ok(result.response.clone()),
            Ending::Finished(Err(e)) => Err(eyre::eyre!("{e:#}")),
            Ending::Stopped => Err(eyre::eyre!("the turn was stopped")),
            Ending::Failed(e) => Err(eyre::eyre!("{e}")),
        };
        for message in conducted
            .closing(service)
            .into_iter()
            .filter_map(|progress| {
                crate::gateway::chat_progress_to_broadcast(progress, &session_id, false)
            })
        {
            let _ = tx.send(message);
        }
        drop(live);
        Ok((session_id, answer?))
    }
}
