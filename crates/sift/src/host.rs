//! What a program needs from the process that runs it.

use std::time::Duration;

/// The runner's side of a program: which tools exist, and how tool calls and
/// SQL blocks run. Calls are polled on the task that awaits [`crate::run`],
/// so task-locals the host relies on are in scope.
#[async_trait::async_trait]
pub trait Host: Send + Sync {
    /// Whether a model-facing tool name is callable by this program
    /// (resident or loaded, and allowed).
    fn has_tool(&self, name: &str) -> bool;

    /// Every callable tool name, for "did you mean" suggestions.
    fn tool_names(&self) -> Vec<String>;

    /// Run one tool call. `Err` is the message the run fails with: a policy
    /// denial or the tool's failure text.
    async fn call(&self, tool: &str, args: serde_json::Value) -> Result<serde_json::Value, String>;

    /// Run one SQL block: the query text with `{{pN}}` placeholders, and each
    /// placeholder's name (`pN`) and value in order. Returns rows as a JSON
    /// array of objects.
    async fn sql(
        &self,
        db: serde_json::Value,
        query: &str,
        params: Vec<(String, serde_json::Value)>,
    ) -> Result<serde_json::Value, String>;

    /// Read a stored table (a large SQL result, another tool's staged
    /// output, an uploaded sheet) by its artifact id, as a JSON array of
    /// row objects. Hosts without artifacts refuse.
    async fn read_artifact(&self, id: &str) -> Result<serde_json::Value, String> {
        let _ = id;
        Err("this install has no artifact store".to_string())
    }

    /// Save rows as a file the user receives (`.xlsx`, or `.csv` when `name`
    /// ends with it) without the rows passing through the model. Returns
    /// what the program may report about the file.
    async fn write_artifact(
        &self,
        name: &str,
        rows: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let _ = (name, rows);
        Err("this install has no artifact store".to_string())
    }
}

/// Budgets for one run. Hitting any of them ends the run with a diagnostic
/// that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Statements and expression evaluations; bulk library work over a list
    /// costs a step per element.
    pub max_steps: u64,
    /// Tool calls and SQL blocks together.
    pub max_calls: u32,
    /// Host calls in flight at once, across every `all(...)`.
    pub max_concurrency: usize,
    /// Wall-clock time for the whole run, host calls included.
    pub deadline: Duration,
    /// Size of the return value as compact JSON.
    pub max_return_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 200_000,
            max_calls: 60,
            max_concurrency: 4,
            deadline: Duration::from_secs(120),
            max_return_bytes: 60_000,
        }
    }
}
