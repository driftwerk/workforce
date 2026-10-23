//! The shapes a monitor shows the hub: its summary, status and probes.

use workforce_core::access::Visibility;
use workforce_orchestrator::monitor::definition::SURFACE_ANY;
use workforce_orchestrator::monitor::MonitorKey;
use workforce_orchestrator::monitor::{ChannelBlock, PollStatus};

/// Why a monitor is or is not watching right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonitorStatus {
    Active,
    Paused,
    /// The watching agent's bot is not a member of the channel.
    AwaitingInvite,
    /// Last edited by a non-owner; the owner must re-save.
    AwaitingOwner,
    /// Names an agent this hub does not run.
    UnknownAgent,
    /// Its source's last poll failed.
    Failing,
}

/// A monitor in a listing.
#[derive(Clone, serde::Serialize)]
pub struct MonitorSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `channel` or `source`.
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// The tool a source monitor polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The filter's kind, or the condition's.
    pub filter: String,
    pub action: String,
    pub enabled: bool,
    pub owner: Option<String>,
    pub visibility: Visibility,
    /// The watching agent, resolved to the default when the definition names none.
    pub agent: Option<String>,
    pub status: MonitorStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<ChannelBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll: Option<PollStatus>,
}

impl MonitorSummary {
    /// Whether `block` describes this monitor as it is bound right now (same
    /// definition, watching agent and channel); a stale row is ignored.
    pub fn is_blocked_by(&self, block: &ChannelBlock) -> bool {
        block.key.monitor_id == self.id
            && block.key.owner == self.owner
            && self
                .agent
                .as_deref()
                .is_none_or(|agent| agent == block.agent)
            && self.channel.as_deref() == Some(block.channel.as_str())
            && self
                .surface
                .as_deref()
                .is_some_and(|surface| surface == SURFACE_ANY || surface == block.surface)
    }

    /// Overlay a current block: an otherwise active monitor is awaiting an invite.
    pub fn with_block(mut self, block: ChannelBlock) -> Self {
        if self.status == MonitorStatus::Active {
            self.status = MonitorStatus::AwaitingInvite;
        }
        self.block = Some(block);
        self
    }

    /// Overlay a source's last poll: an otherwise active monitor whose poll
    /// failed is failing.
    pub fn with_poll(mut self, poll: PollStatus) -> Self {
        if self.status == MonitorStatus::Active && poll.error.is_some() {
            self.status = MonitorStatus::Failing;
        }
        self.poll = Some(poll);
        self
    }

    pub fn key(&self) -> MonitorKey {
        MonitorKey::new(&self.id, self.owner.as_deref())
    }
}

/// What one read of a watched channel found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelProbe {
    Readable,
    /// The watching agent's bot must be invited; carries the surface's error.
    NotAMember(String),
    Failed(String),
}

/// What one poll of a source found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PollReport {
    /// Keys observed, fired or not.
    pub seen: usize,
    /// Dispatches this poll made.
    pub fired: usize,
    pub error: Option<String>,
}

/// What a save or resume learns by reading its monitor's source right away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Channel(ChannelProbe),
    Source(PollReport),
}
