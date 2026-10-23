use eyre::Result;

/// Cross-task gate deciding, for one inbound Slack message, whether *this* hub
/// task is the one that should reply.
///
/// During a zero-downtime deploy two hub tasks are connected to Slack at once,
/// and Slack load-balances the `message`/`app_mention` pair across their
/// sockets — so without a shared gate both tasks reply. A backing store makes
/// the claim atomic across processes, so exactly one task answers each message.
#[async_trait::async_trait]
pub trait SlackMessageClaimer: Send + Sync {
    /// Atomically claim `(agent_id, workspace_id, channel, ts)`, returning `true`
    /// for the single winner and `false` for any task that finds it already
    /// claimed.
    async fn try_claim(
        &self,
        agent_id: &str,
        workspace_id: &str,
        channel: &str,
        ts: &str,
    ) -> Result<bool>;

    /// Give a claim back when the claimed message's turn never ran (setup
    /// failure, turn-claim conflict), so a redelivery of the same event can be
    /// processed instead of being dropped until the prune TTL.
    async fn release(
        &self,
        _agent_id: &str,
        _workspace_id: &str,
        _channel: &str,
        _ts: &str,
    ) -> Result<()> {
        Ok(())
    }
}
