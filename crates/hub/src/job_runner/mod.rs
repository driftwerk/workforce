//! Async job runner — background tool execution with NATS notification.
//!
//! Provides four job types:
//! - **Tool jobs**: Execute any registered tool asynchronously.
//! - **Shell jobs**: Execute shell commands (wraps exec.run).
//! - **Monitor jobs**: Poll a tool until a condition is met.
//! - **Agent jobs**: Run an isolated LLM sub-agent (via the `AgentExecutor`).
//!
//! Extended capabilities:
//! - **Job chaining / DAGs**: Declare dependency graphs of jobs with
//!   parallel execution and cascading cancellation.
//! - **Retry policies**: Automatic retry with configurable backoff
//!   strategies (exponential, linear, fixed).
//! - **Output streaming**: Real-time job output via NATS progress events
//!   with throttled updates and session injection.
//!
//! Jobs run in background tokio tasks with a configurable concurrency limit
//! (default 5). Results are persisted to disk and notifications are published
//! via NATS JetStream for reliable delivery.
//!
//! ## Stats Integration
//!
//! Job execution metrics (duration, success/failure, tool invocations) are
//! recorded to the workforce stats system via the `StatsRecorder` trait.
//! The concrete `JobStatsRecorder` in `serve.rs` feeds into
//! `CostTracker`/`SqliteStore`.
//!
//! ## Session Notification
//!
//! Job completions are injected into active agent sessions via the
//! `SessionInjector` trait. The concrete `JobSessionInjector` in `serve.rs`
//! uses the `active_runners` map from `AgentService`.

pub mod chain;
pub mod monitor;
pub mod persistence;
pub mod recovery;
pub mod retry;
pub mod runner;
pub mod streaming;
pub mod types;

pub use chain::{ChainSummary, StepSummary};
pub use persistence::JobPersistence;
pub use runner::{
    AgentExecutor, AgentRunResult, JobRunner, NotificationSink, SessionInjector, StatsRecorder,
    ToolExecutor,
};
pub use types::*;
