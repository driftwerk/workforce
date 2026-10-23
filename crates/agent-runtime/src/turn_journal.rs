//! A turn's record of its tool calls as they happen, so a turn that dies with
//! its process is taken up where it stopped rather than rerun: the calls that
//! returned are replayed, and work a call left running is collected.

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;

use crate::runner::ToolInvocation;

/// What a collected call answers when its tool cannot say how it ended.
pub const OUTCOME_UNKNOWN: &str = "This call was interrupted by a restart and its outcome is \
     unknown. Check what it did before repeating it.";

/// Where a turn records its tool calls as they happen.
#[async_trait]
pub trait TurnJournal: Send + Sync {
    /// Call `call` started work that outlives this process; `input` is the
    /// call's input as the transcript keeps it, and `plan` is what its tool
    /// needs to collect that work later.
    async fn pending(
        &self,
        call: &str,
        tool: &str,
        input: &serde_json::Value,
        plan: serde_json::Value,
    ) -> Result<()>;

    /// Call `call`'s work reached `mark`.
    async fn checkpoint(&self, call: &str, mark: &str) -> Result<()>;

    /// Call `call` returned. Recording it is best effort: the turn's own
    /// save still keeps the call if the journal cannot.
    async fn finished(&self, call: &str, invocation: &ToolInvocation);
}

/// Work an interrupted turn left running, to collect before the next turn's
/// first round.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingCall {
    pub call: String,
    pub tool: String,
    /// As the transcript keeps it, secrets as markers.
    pub input: serde_json::Value,
    pub plan: serde_json::Value,
    /// The last mark the work reached, if any.
    pub checkpoint: Option<String>,
}

/// The tool call a tool is executing in: its id, its input as the transcript
/// keeps it, and the journal of the turn it belongs to.
#[derive(Clone)]
pub struct ToolCall {
    id: String,
    input: serde_json::Value,
    journal: Option<Arc<dyn TurnJournal>>,
}

tokio::task_local! {
    static CURRENT_TOOL_CALL: ToolCall;
}

impl ToolCall {
    /// A new call of the turn journalled by `journal`.
    pub fn new(journal: Option<Arc<dyn TurnJournal>>, input: serde_json::Value) -> Self {
        Self::resumed(uuid::Uuid::new_v4().to_string(), journal, input)
    }

    /// Call `id` again, to collect what it left running.
    pub fn resumed(
        id: String,
        journal: Option<Arc<dyn TurnJournal>>,
        input: serde_json::Value,
    ) -> Self {
        Self { id, input, journal }
    }

    /// The call the running tool was invoked in; `None` outside a turn.
    pub fn current() -> Option<Self> {
        CURRENT_TOOL_CALL.try_with(Clone::clone).ok()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Run `fut` as this call's tool execution.
    pub async fn scope<F: Future>(self, fut: F) -> F::Output {
        CURRENT_TOOL_CALL.scope(self, fut).await
    }

    /// Record that this call started work that outlives the process. A turn
    /// without a journal has nowhere to record it, and nothing to resume.
    pub async fn pending(&self, tool: &str, plan: serde_json::Value) -> Result<()> {
        match &self.journal {
            Some(journal) => journal.pending(&self.id, tool, &self.input, plan).await,
            None => Ok(()),
        }
    }

    /// Record that this call's work reached `mark`, before acting on it.
    pub async fn checkpoint(&self, mark: &str) -> Result<()> {
        match &self.journal {
            Some(journal) => journal.checkpoint(&self.id, mark).await,
            None => Ok(()),
        }
    }

    pub(crate) async fn finished(&self, invocation: &ToolInvocation) {
        if let Some(journal) = &self.journal {
            journal.finished(&self.id, invocation).await;
        }
    }
}
