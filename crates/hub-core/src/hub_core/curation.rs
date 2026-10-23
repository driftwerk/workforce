//! What a curation run carries: the context the self-learning records are kept under.

use std::sync::atomic::AtomicU32;
use std::sync::Arc;

/// Per-curation-turn context the record tool persists with. The counter is an
/// `Arc<AtomicU32>` so `max_items_per_run` is enforced across the turn's many
/// `record` calls, staying shared as the tool clones the ctx per call.
#[derive(Clone)]
pub struct CurationCtx {
    pub agent_id: String,
    pub plan_id: String,
    pub max_items: u32,
    pub freshness_days: u32,
    pub recorded: Arc<AtomicU32>,
}

impl CurationCtx {
    pub fn new(agent_id: String, plan_id: String, max_items: u32, freshness_days: u32) -> Self {
        Self {
            agent_id,
            plan_id,
            max_items,
            freshness_days,
            recorded: Arc::new(AtomicU32::new(0)),
        }
    }
}
