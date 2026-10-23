//! Hub-side orchestration for the self-learning producer — the scheduled,
//! read-only curation loop that turns a learning plan's named sources into
//! atomic, primary-cited knowledge items.
//!
//! Storage and the item lifecycle live in `workforce-self-learning`; this
//! module hosts the runtime: the store-lifecycle accessors on
//! [`crate::serve::AgentService`], the read-only `Curator` curation turn
//! (`run_isolated`), the `self_learning.record` agent tool + reconciliation,
//! the background [`runner::SelfLearningRunner`], and the
//! `## Self-Learned Knowledge` context surfacing.

use crate::serve::AgentService;

pub mod backend;
pub mod deep;
pub mod experiential;
pub(crate) mod quality_gate;
pub mod record;
pub mod record_node;
pub mod review;
pub mod review_node;
pub mod runner;

pub use backend::{open_self_learning_store, select_backend, CurationStores};
pub use record::{
    reconcile, CurationCtx, RecordAction, SelfLearningRecordTool, SELF_LEARNING_CURATION,
};
pub use record_node::DeepNodeRecordTool;
pub use runner::{SelfLearningRunner, SelfLearningWakeHandle};

/// Tag attached to every Memory row that backs a self-learned knowledge item.
/// The recall path uses this tag both to build the `## Self-Learned Knowledge`
/// block and to exclude these items from generic recall. Mirrors
/// [`crate::training::TRAINING_TAG`].
pub const SELF_LEARNED_TAG: &str = "self_learned";

/// Tag attached to every Memory row that backs a deep-expertise tree node,
/// distinct from [`SELF_LEARNED_TAG`] so deep recall (the `## Deep Expertise`
/// block) and atomic-item recall (the `## Self-Learned Knowledge` block) never
/// cross-pollute: a node's large multi-paragraph body must not consume the
/// atomic block's top-k, and an atomic claim must not surface as a node.
pub const DEEP_EXPERTISE_TAG: &str = "deep_expertise";

/// Tag prefix carrying the originating plan id, so a future filter can target
/// one plan's items.
pub const PLAN_TAG_PREFIX: &str = "plan:";

/// Tag prefix carrying the item's topic.
pub const TOPIC_TAG_PREFIX: &str = "topic:";

/// Tag prefix carrying the source skill/kind, surfaced as provenance in the
/// rendered context line.
pub const SOURCE_TAG_PREFIX: &str = "src:";

/// Prefix of the task label (and cost-row task type) of a plan's runs,
/// followed by the plan id.
pub const TASK_LABEL_PREFIX: &str = "self_learning:";

mod surface;
pub use surface::{render_deep_expertise, render_self_learned};

/// Open the self-learning store if it is effective-enabled and not yet
/// open. Idempotent; used by the runner each tick and by `reload_config`
/// on a hot-enable so global-off + per-agent-on works with no restart.
/// Populates both the atomic-item and deep-expertise views in lockstep.
pub(crate) async fn ensure_store(service: &AgentService) {
    if !service.self_learning_enabled() || service.self_learning_store.read().is_some() {
        return;
    }
    let cfg = service.effective_self_learning();
    match crate::self_learning::backend::open_effective(&cfg, &service.config).await {
        Ok(Some(stores)) => {
            let mut guard = service.self_learning_store.write();
            if guard.is_none() {
                *guard = Some(stores.self_learning);
                *service.deep_expertise_store.write() = Some(stores.deep_expertise);
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(error = %e, "self-learning store unavailable; leaving disabled this cycle")
        }
    }
}
