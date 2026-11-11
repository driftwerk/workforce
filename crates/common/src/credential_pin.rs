//! Which vault tiers a connector's credentials come from for one agent, on
//! a hub with `[desk]`: its own connection (`AgentPerUser`, then
//! `AgentSpecific`), the person's for all their agents (`GlobalPerUser`,
//! then `Global`), or none for a connector that is off. Every other read
//! resolves across all four tiers as before ([`TierPin::All`]).
//!
//! The pin is read per call, from the agent's `[skills]`, through a
//! [`PinSlot`] the agent's tool registry holds and every credential store
//! its adapters load with keeps: a slot that is never filled pins nothing,
//! which is every hub without `[desk]`.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::settings::VaultSource;

/// The tiers one read may come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TierPin {
    /// All four, in their order: every read that isn't a pinned connector's.
    #[default]
    All,
    /// "My connection": the person's, for all their agents.
    Mine,
    /// "Its own connection": just this agent's.
    Own,
    /// The connector is off: nothing resolves.
    Nothing,
}

impl TierPin {
    pub fn admits(self, tier: VaultSource) -> bool {
        match self {
            Self::All => true,
            Self::Nothing => false,
            Self::Mine => matches!(tier, VaultSource::GlobalPerUser | VaultSource::Global),
            Self::Own => matches!(tier, VaultSource::AgentPerUser | VaultSource::AgentSpecific),
        }
    }
}

/// Decides the pin of a logical vault path for one agent.
pub trait PinSource: Send + Sync {
    fn pin(&self, path: &str) -> TierPin;
    /// The instance of `connector` a call naming none gets, where the agent
    /// names one.
    fn default_instance(&self, _connector: &str) -> Option<String> {
        None
    }
    /// Whether instance `instance` of `connector` only reads for the agent.
    fn read_only_instance(&self, _connector: &str, _instance: &str) -> bool {
        false
    }
}

/// Where an agent's [`PinSource`] is put once its hub knows it runs Desk.
#[derive(Default)]
pub struct PinSlot(RwLock<Option<Arc<dyn PinSource>>>);

impl PinSlot {
    pub fn fill(&self, source: Arc<dyn PinSource>) {
        *self.0.write() = Some(source);
    }

    /// The pin of `path`: [`TierPin::All`] while the slot is empty.
    pub fn pin(&self, path: &str) -> TierPin {
        self.0.read().as_ref().map_or(TierPin::All, |s| s.pin(path))
    }

    pub fn is_filled(&self) -> bool {
        self.0.read().is_some()
    }

    /// [`PinSource::default_instance`]: none while the slot is empty.
    pub fn default_instance(&self, connector: &str) -> Option<String> {
        self.0
            .read()
            .as_ref()
            .and_then(|s| s.default_instance(connector))
    }

    /// [`PinSource::read_only_instance`]: none while the slot is empty.
    pub fn read_only_instance(&self, connector: &str, instance: &str) -> bool {
        self.0
            .read()
            .as_ref()
            .is_some_and(|s| s.read_only_instance(connector, instance))
    }
}

/// A connector's write tool running for one agent: the instance its
/// adapter resolves is checked against the agent's read-only connections
/// ([`write_refused`]).
#[derive(Clone)]
pub struct Writing {
    pub connector: String,
    pub pins: Arc<PinSlot>,
}

tokio::task_local! {
    static WRITING: Writing;
}

impl Writing {
    /// Runs `fut` as this write.
    pub async fn scope<F: std::future::Future>(self, fut: F) -> F::Output {
        WRITING.scope(self, fut).await
    }
}

/// Whether the call in scope is a write of `connector`'s to an instance
/// that only reads for its agent.
pub fn write_refused(connector: &str, instance: &str) -> bool {
    WRITING
        .try_with(|w| w.connector == connector && w.pins.read_only_instance(connector, instance))
        .unwrap_or(false)
}

impl std::fmt::Debug for PinSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinSlot")
            .field("filled", &self.is_filled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Globex;
    impl PinSource for Globex {
        fn pin(&self, _: &str) -> TierPin {
            TierPin::All
        }
        fn read_only_instance(&self, connector: &str, instance: &str) -> bool {
            (connector, instance) == ("jira", "globex")
        }
    }

    #[tokio::test]
    async fn only_a_write_to_a_read_only_connection_is_refused() {
        let pins = Arc::new(PinSlot::default());
        assert!(
            !pins.read_only_instance("jira", "globex"),
            "an empty slot pins nothing"
        );
        pins.fill(Arc::new(Globex));
        // Outside a write, nothing is refused.
        assert!(!write_refused("jira", "globex"));
        let writing = Writing {
            connector: "jira".into(),
            pins: Arc::clone(&pins),
        };
        writing
            .scope(async {
                assert!(write_refused("jira", "globex"));
                assert!(!write_refused("jira", "acme"));
                assert!(!write_refused("linear", "globex"));
            })
            .await;
    }

    #[test]
    fn each_pin_admits_its_tiers() {
        use VaultSource::*;
        let tiers = [AgentPerUser, GlobalPerUser, AgentSpecific, Global];
        let admitted = |pin: TierPin| {
            tiers
                .iter()
                .copied()
                .filter(|t| pin.admits(*t))
                .collect::<Vec<_>>()
        };
        assert_eq!(admitted(TierPin::All), tiers);
        assert_eq!(admitted(TierPin::Mine), [GlobalPerUser, Global]);
        assert_eq!(admitted(TierPin::Own), [AgentPerUser, AgentSpecific]);
        assert!(admitted(TierPin::Nothing).is_empty());
    }

    #[test]
    fn an_empty_slot_pins_nothing() {
        struct Off;
        impl PinSource for Off {
            fn pin(&self, _: &str) -> TierPin {
                TierPin::Nothing
            }
        }
        let slot = PinSlot::default();
        assert_eq!(slot.pin("skills/linear/api_key"), TierPin::All);
        slot.fill(Arc::new(Off));
        assert_eq!(slot.pin("skills/linear/api_key"), TierPin::Nothing);
    }
}
