pub mod access;
pub mod approval;
pub mod arguments;
pub mod chart;
pub mod config_guard;
pub mod data;
pub mod database;
pub mod diagram;
pub mod dispatch;
pub mod exec;
pub mod exec_runner;
pub mod export_csv;
pub mod export_xlsx;
pub mod filesystem;
#[cfg(test)]
pub(crate) mod goldens;
pub mod http;
pub mod image_gen;
pub mod image_view;
pub mod loading;
pub mod paths;
pub mod program;
pub mod registry;
pub mod report;
pub mod security;
pub mod speech_gen;
pub mod time;
pub mod video_compose;
pub mod video_gen;
pub mod web;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};

/// The result of executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the tool execution succeeded.
    pub success: bool,
    /// The output of the tool.
    pub output: serde_json::Value,
    /// Error message if the tool failed.
    pub error: Option<String>,
    /// Set when the upstream refused the call for rate limiting before
    /// executing it — retrying cannot duplicate side effects. Absent means
    /// unknown, which no caller may treat as retryable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited: Option<RateLimit>,
    /// Execution time in milliseconds.
    pub duration_ms: u64,
    /// Tools this call made callable in full (internal names), such as the
    /// tools a skill it enabled registered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loads: Vec<String>,
}

/// A refusal-for-rate-limiting signal, carrying the server's Retry-After
/// hint when it sent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    pub retry_after_secs: Option<u64>,
}

/// Typed error an adapter raises where it maps an HTTP 429 (or an
/// equivalent refusal), so the signal survives the eyre chain to
/// [`ToolResult::from_report`]. `message` keeps the provider's own wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitedError {
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

impl RateLimitedError {
    pub fn new(message: impl Into<String>, retry_after_secs: Option<u64>) -> Self {
        Self {
            message: message.into(),
            retry_after_secs,
        }
    }
}

impl std::fmt::Display for RateLimitedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.retry_after_secs {
            Some(secs) => write!(f, "{} (retry after {secs}s)", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for RateLimitedError {}

/// Definition of a tool that an agent can use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Unique tool name (e.g., "filesystem.read").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
}

/// What a tool call does to a messaging surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceEffect {
    Reads,
    Writes {
        platform: String,
        destination: String,
    },
}

impl SurfaceEffect {
    /// A write of an email to whom `input` addresses it.
    pub fn email(input: &serde_json::Value) -> Self {
        let to = match &input["to"] {
            serde_json::Value::String(to) => to.clone(),
            serde_json::Value::Array(to) => to
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            _ => String::new(),
        };
        Self::Writes {
            platform: "email".to_string(),
            destination: to,
        }
    }
}

/// Trait that all tools must implement.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's unique name.
    fn name(&self) -> &str;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// JSON schema describing the expected input.
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given input.
    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult>;

    /// An optional soft execution deadline. When `Some`, the runner drops the
    /// in-flight call if it overruns and hands the model a structured timeout
    /// error, so one runaway tool can't freeze a turn (the 700s `code.search`
    /// case). Returning control this way is also what re-creates the loop
    /// boundary the periodic check-in relies on.
    ///
    /// Leave as the `None` default for tools that legitimately run long —
    /// coding sessions, the advisor, sub-agents, the browser — which pace
    /// themselves and surface their own progress. Any deadline set here MUST be
    /// shorter than the runner's check-in interval, or the "max silence"
    /// guarantee is a lie.
    fn soft_deadline(&self) -> Option<std::time::Duration> {
        None
    }

    /// An optional one-line, human-readable summary of THIS call's scope, used
    /// in an approval prompt in place of the generic parameter-shape summary.
    /// Lets a tool tell an approver what a single approval actually authorizes
    /// — e.g. a bulk reader reporting "read up to 364 object(s) from bucket
    /// `x`" rather than just the bucket name. Must stay cheap: it runs on every
    /// call (it also feeds the debug log), not only when approval is needed, so
    /// derive it from the raw input without doing the tool's real work.
    /// Defaults to `None`, leaving the generic shape-based summary in place.
    fn approval_summary(&self, _input: &serde_json::Value) -> Option<String> {
        None
    }

    /// When a tool holds itself behind a human decision whatever the hub's
    /// policy says, the reason an approval prompt carries. The dispatch gate
    /// composes this **last** — after policy, the capability gate and the
    /// permission cascade — so a tool can only ever ask for more approval
    /// than policy requires, never less, and no saved Always-Allow can
    /// pre-empt it. Like [`Tool::approval_summary`] it runs on every
    /// dispatch, so it must be a cheap read of already-loaded state.
    /// Defaults to `None`: policy alone decides.
    fn approval_requirement(&self) -> Option<String> {
        None
    }

    /// What THIS call does to a messaging surface: reads it, or writes to a
    /// destination on a platform. The cross-surface gate reads it to hold a
    /// write outside the Desk conversation a person is in for their say.
    /// `None`, the default, leaves the gate to the tool's namespace.
    fn surface_effect(&self, _input: &serde_json::Value) -> Option<SurfaceEffect> {
        None
    }

    /// Collect what a call of this tool left running when its turn was
    /// interrupted: `input` is the call's input, `plan` what the call
    /// journalled, and `checkpoint` the last mark its work reached. Only a
    /// tool that journals pending work overrides this.
    async fn collect(
        &self,
        _input: serde_json::Value,
        _plan: serde_json::Value,
        _checkpoint: Option<String>,
    ) -> Result<ToolResult> {
        Ok(ToolResult::failure(crate::turn_journal::OUTCOME_UNKNOWN, 0))
    }

    /// End what an interrupted call left running, when it is no longer to be
    /// collected. Best effort; the work otherwise ends at its own deadline.
    async fn abandon(&self, _plan: serde_json::Value, _checkpoint: Option<String>) {}

    /// Get the full tool definition.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

impl ToolResult {
    pub fn success(output: serde_json::Value, duration_ms: u64) -> Self {
        Self {
            success: true,
            output,
            error: None,
            rate_limited: None,
            duration_ms,
            loads: Vec::new(),
        }
    }

    pub fn failure(error: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            success: false,
            output: serde_json::Value::Null,
            error: Some(error.into()),
            rate_limited: None,
            duration_ms,
            loads: Vec::new(),
        }
    }

    /// A refusal-for-rate-limiting failure carrying the typed signal.
    pub fn rate_limited(
        error: impl Into<String>,
        retry_after_secs: Option<u64>,
        duration_ms: u64,
    ) -> Self {
        Self {
            rate_limited: Some(RateLimit { retry_after_secs }),
            ..Self::failure(error, duration_ms)
        }
    }

    /// [`Self::failure`] that preserves a [`RateLimitedError`] found anywhere
    /// in the report's chain, so `wrap_err` context doesn't strip the signal.
    pub fn from_report(report: &eyre::Report, duration_ms: u64) -> Self {
        let message = format!("{report:#}");
        match report
            .chain()
            .find_map(|e| e.downcast_ref::<RateLimitedError>())
        {
            Some(limit) => Self::rate_limited(message, limit.retry_after_secs, duration_ms),
            None => Self::failure(message, duration_ms),
        }
    }

    /// A lookup that ran correctly and found nothing.
    ///
    /// The third outcome, and the one tools kept getting wrong: "no files in
    /// this conversation" is not a failed action, it is a **verified fact**
    /// about the conversation. Reporting it as a failure made the post-turn
    /// notice tell the reader that "claims may be wrong" on a turn whose answer
    /// — "you didn't attach anything" — was exactly right, and was itself
    /// derived from this result.
    ///
    /// Use this whenever the tool did its job and the answer is empty. Keep
    /// [`Self::failure`] for an action that did NOT happen: a timeout, a denied
    /// credential, a 500 — cases where anything said about the outcome really
    /// is unverified. The rule of thumb is whether a human would answer "there
    /// aren't any" (empty) or "I couldn't check" (failure).
    ///
    /// Succeeds, so it is never flagged as unrecovered, and carries
    /// `found: false` so a caller can branch without parsing prose.
    pub fn empty(message: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            success: true,
            output: serde_json::json!({ "found": false, "message": message.into() }),
            error: None,
            rate_limited: None,
            duration_ms,
            loads: Vec::new(),
        }
    }

    /// This result, having made `tools` callable in full.
    pub fn loading(self, tools: Vec<String>) -> Self {
        Self {
            loads: tools,
            ..self
        }
    }
}

#[cfg(test)]
mod tool_result_tests {
    use super::*;

    /// An empty lookup is a success carrying a fact, not a failure. It must
    /// leave `error` unset: the post-turn notice reads exactly that field, and
    /// an empty result with an error is what told readers to doubt a correct
    /// "you didn't attach anything".
    #[test]
    fn empty_is_a_success_that_reports_finding_nothing() {
        let r = ToolResult::empty("no files found in this conversation", 3);
        assert!(r.success, "a lookup that ran is not a failed action");
        assert!(r.error.is_none(), "no error, or the turn gets flagged");
        assert_eq!(r.output["found"], false, "machine-readable, not prose");
        assert_eq!(r.output["message"], "no files found in this conversation");
    }

    /// The distinction the constructor exists to preserve.
    #[test]
    fn failure_still_reports_an_action_that_did_not_happen() {
        let r = ToolResult::failure("connection timed out", 3);
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("connection timed out"));
        assert!(r.rate_limited.is_none());
    }

    #[test]
    fn rate_limited_error_display_appends_hint_only_when_present() {
        let with = RateLimitedError::new("Rate limited by Datadog: quota", Some(30));
        assert_eq!(
            with.to_string(),
            "Rate limited by Datadog: quota (retry after 30s)"
        );
        let without = RateLimitedError::new("Rate limited by Jira", None);
        assert_eq!(without.to_string(), "Rate limited by Jira");
    }

    #[test]
    fn from_report_extracts_the_signal_through_a_wrap_err_chain() {
        let report = eyre::Report::new(RateLimitedError::new("throttled", Some(12)))
            .wrap_err("Jira API request failed");
        let r = ToolResult::from_report(&report, 5);
        assert!(!r.success);
        assert_eq!(
            r.rate_limited,
            Some(RateLimit {
                retry_after_secs: Some(12)
            })
        );
        assert_eq!(
            r.error.as_deref(),
            Some("Jira API request failed: throttled (retry after 12s)")
        );
    }

    #[test]
    fn from_report_on_a_plain_error_carries_no_signal() {
        let report = eyre::eyre!("HTTP 500 internal error");
        let r = ToolResult::from_report(&report, 5);
        assert!(!r.success);
        assert!(r.rate_limited.is_none());
        assert_eq!(r.error.as_deref(), Some("HTTP 500 internal error"));
    }

    /// Prose alone must never read as the typed signal: a message merely
    /// containing throttle wording (a partial failure whose side effects
    /// committed) stays untyped and therefore unretryable.
    #[test]
    fn prose_about_rate_limits_is_not_the_signal() {
        let report = eyre::eyre!("created 3 records, then upstream rate limited (429)");
        assert!(ToolResult::from_report(&report, 1).rate_limited.is_none());
    }

    #[test]
    fn serde_is_additive_in_both_directions() {
        let old: ToolResult = serde_json::from_str(
            r#"{"success":false,"output":null,"error":"boom","duration_ms":1}"#,
        )
        .unwrap();
        assert!(old.rate_limited.is_none());

        let plain = serde_json::to_value(ToolResult::failure("boom", 1)).unwrap();
        assert!(plain.get("rate_limited").is_none());

        let typed =
            serde_json::to_value(ToolResult::rate_limited("slow down", Some(7), 1)).unwrap();
        assert_eq!(typed["rate_limited"]["retry_after_secs"], 7);
    }
}
