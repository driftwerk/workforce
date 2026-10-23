//! Fleet-wide per-session turn claim.
//!
//! Wraps [`SessionStore::try_claim_turn`] in an RAII guard so every ingress
//! path (Slack, gateway, Teams, NATS chat worker) that funnels through
//! `AgentService::process_message{,_with_progress}` holds exactly one live
//! turn per session across all hub instances. On the single-process stores the
//! underlying claim always grants, so this is free outside Postgres.

use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use tracing::warn;
use workforce_common::session::SessionStore;

/// How long a claim stays valid without renewal. Generous enough for a long
/// coding turn between renewals; short enough that a crashed holder's session
/// becomes claimable within minutes.
const TURN_CLAIM_TTL_SECS: u64 = 900;

use crate::hub_core::live_turns::TURN_STILL_RUNNING;

/// Refusal because another hub instance holds a live claim for the session.
/// Carries what the store knows about the holder so callers can render an
/// honest wait ("frees within ~Ns") or defer instead of failing.
#[derive(Debug)]
pub(crate) struct TurnClaimHeld {
    pub holder: Option<String>,
    pub expires_in_secs: Option<u64>,
}

impl std::fmt::Display for TurnClaimHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(TURN_STILL_RUNNING)?;
        if let Some(secs) = self.expires_in_secs {
            write!(f, " (frees within ~{secs}s)")?;
        }
        Ok(())
    }
}

impl std::error::Error for TurnClaimHeld {}

/// RAII holder of a session's fleet-wide turn claim. Renews in the background
/// (at TTL/3) for turns that outlive the TTL and releases on drop — covering
/// early returns, errors, and panics alike. A release with a superseded claim
/// id is a no-op at the store, so dropping late can never evict a successor.
pub(crate) struct SessionTurnClaim {
    sessions: Arc<dyn SessionStore>,
    session_id: String,
    claim_id: String,
    renewer: tokio::task::JoinHandle<()>,
}

impl SessionTurnClaim {
    /// Try to take the claim for `session_id`. `Ok(None)` means another hub
    /// instance holds a live claim — the caller must not run the turn.
    pub(crate) fn try_acquire(
        sessions: &Arc<dyn SessionStore>,
        session_id: &str,
    ) -> Result<Option<Self>> {
        // A draining process must not take new claims: the shutdown's bulk
        // release has to be the last word, and a turn started now could not
        // finish anyway.
        if crate::shutdown::in_progress() {
            eyre::bail!("hub is shutting down — turn not started");
        }
        let claim_id = uuid::Uuid::new_v4().to_string();
        if !sessions.try_claim_turn(session_id, &claim_id, TURN_CLAIM_TTL_SECS)? {
            return Ok(None);
        }
        Ok(Some(Self::spawn_renewer(sessions, session_id, claim_id)))
    }

    /// [`Self::try_acquire`], but refusal becomes a typed [`TurnClaimHeld`]
    /// error enriched with the holder read back from the store.
    pub(crate) fn acquire_or_held(
        sessions: &Arc<dyn SessionStore>,
        session_id: &str,
    ) -> Result<Self> {
        Self::try_acquire(sessions, session_id)?.ok_or_else(|| {
            let (holder, expires_in_secs) = sessions
                .turn_claim_holder(session_id)
                .ok()
                .flatten()
                .map(|(h, s)| (Some(h), Some(s)))
                .unwrap_or((None, None));
            eyre::Report::new(TurnClaimHeld {
                holder,
                expires_in_secs,
            })
        })
    }

    pub(crate) fn claim_id(&self) -> &str {
        &self.claim_id
    }

    /// Release the claim now and wait for the store to confirm, for a caller
    /// whose next step must not find the row (a stop the user is about to
    /// follow with a new message). The guard's own release on drop is then a
    /// no-op.
    pub(crate) async fn release(&self) {
        self.renewer.abort();
        release_claim(&self.sessions, &self.session_id, &self.claim_id).await;
    }

    /// Fail a turn still in setup once `cancel` is set, releasing the claim
    /// first so the user's next message is not refused on its account.
    pub(crate) async fn unless_cancelled(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            self.release().await;
            eyre::bail!("turn cancelled during setup");
        }
        Ok(())
    }

    fn spawn_renewer(sessions: &Arc<dyn SessionStore>, session_id: &str, claim_id: String) -> Self {
        let renewer = {
            let sessions = Arc::clone(sessions);
            let session_id = session_id.to_string();
            let claim_id = claim_id.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(TURN_CLAIM_TTL_SECS / 3));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    // After the shutdown bulk release every renewal would
                    // only report the claim gone; the claim's remaining TTL
                    // outlives any drain window.
                    if crate::shutdown::in_progress() {
                        break;
                    }
                    let (s, sid, cid) =
                        (Arc::clone(&sessions), session_id.clone(), claim_id.clone());
                    match tokio::task::spawn_blocking(move || {
                        s.renew_turn_claim(&sid, &cid, TURN_CLAIM_TTL_SECS)
                    })
                    .await
                    {
                        Ok(Ok(true)) => {}
                        Ok(Ok(false)) => {
                            // Superseded by a newer turn's claim, or stolen after
                            // this process stalled past the TTL. The session
                            // save CAS still protects the transcript.
                            warn!(
                                session_id = %session_id,
                                "Session turn claim was superseded — a duplicate turn may be running elsewhere"
                            );
                            break;
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, session_id = %session_id, "Renewing session turn claim failed");
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            sessions: Arc::clone(sessions),
            session_id: session_id.to_string(),
            claim_id,
            renewer,
        }
    }
}

impl Drop for SessionTurnClaim {
    fn drop(&mut self) {
        self.renewer.abort();
        let sessions = Arc::clone(&self.sessions);
        let session_id = std::mem::take(&mut self.session_id);
        let claim_id = std::mem::take(&mut self.claim_id);
        let release = move || release_blocking(sessions.as_ref(), &session_id, &claim_id);
        // Drop can run on a runtime worker; the store call blocks, so hand it
        // to the blocking pool when one exists.
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tokio::task::spawn_blocking(release);
            }
            Err(_) => release(),
        }
    }
}

/// Release `claim_id` on `session_id` and wait for the store. A superseded or
/// already-released id is a no-op, so this is safe beside the guard's own
/// release on drop.
pub(crate) async fn release_claim(
    sessions: &Arc<dyn SessionStore>,
    session_id: &str,
    claim_id: &str,
) {
    let (sessions, session_id, claim_id) = (
        Arc::clone(sessions),
        session_id.to_string(),
        claim_id.to_string(),
    );
    if let Err(e) = tokio::task::spawn_blocking(move || {
        release_blocking(sessions.as_ref(), &session_id, &claim_id)
    })
    .await
    {
        warn!(error = %e, "Releasing session turn claim did not complete");
    }
}

fn release_blocking(sessions: &dyn SessionStore, session_id: &str, claim_id: &str) {
    if let Err(e) = sessions.release_turn_claim(session_id, claim_id) {
        warn!(error = %e, session_id = %session_id, "Releasing session turn claim failed");
    }
}
