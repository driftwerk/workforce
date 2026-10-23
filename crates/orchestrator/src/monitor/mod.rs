//! Monitors: owned, runtime-authored definitions that durably watch a channel
//! or a source and dispatch an action per matching message or observation.
//! Definitions are stored and revisioned in [`crate::workflow_store`] under
//! kind `monitor`; this module holds the definition model, the pure
//! observation logic, and the durability store.

pub mod condition;
pub mod definition;
pub mod event;
pub mod observe;
pub mod store;

pub use condition::CompiledCondition;
pub use definition::{
    Action, Binding, CompiledFilter, Condition, ConditionKind, Decide, Filter, MonitorDefinition,
    MonitorMeta, Source,
};
pub use event::{MonitorEvent, Observation, SOURCE_SURFACE, VALUE_KEY};
pub use observe::{observe, ObservedState, Outcome};
pub use store::{
    ChannelBlock, Claim, DispatchActivity, DispatchKey, MonitorKey, MonitorStore, PollStatus,
    RecoverableDispatch, SourceState,
};
