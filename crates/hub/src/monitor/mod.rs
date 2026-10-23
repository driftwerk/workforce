//! Monitors (hub): the owned definition runtime, the dispatch engine (live
//! tap, catch-up sweep and source poll over the durable claim gate), the
//! governed tool invoker a source is read with, and the agent tools.

pub mod binding_access;
pub mod decide;
pub mod engine;
pub mod invoke;
pub mod runtime;
pub mod tool;
mod wire;

pub use binding_access::{
    linked_platform_ids, BindingAccess, BindingCaller, BindingDenied, PlatformIdFn,
};
pub use decide::{DecideGate, DecisionGate, Verdict};
pub use engine::{
    ActionOutcome, ActionSink, ChannelProbe, MonitorEngine, MonitorEngineConfig, PollReport, Probe,
    WorkflowNotifySink,
};
pub use invoke::{GovernedInvoker, InvokeError, ToolCall, ToolInvoke};
pub use runtime::{
    EngineAgent, KnownAgents, MonitorRuntime, MonitorStatus, MonitorSummary, OwnedMonitor,
};
pub use tool::MonitorTool;
pub(crate) use wire::wire;
