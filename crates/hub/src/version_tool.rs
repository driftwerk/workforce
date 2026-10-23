//! `system.version` tool — reports the deployed build: git commit SHA,
//! image build date, crate version. Lets users ask the agent "what version
//! is currently deployed?" and get the same answer the UI footer shows.

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{Tool, ToolResult};

pub struct SystemVersionTool;

#[async_trait]
impl Tool for SystemVersionTool {
    fn name(&self) -> &str {
        "system.version"
    }

    fn description(&self) -> &str {
        "Show the currently deployed build version: git commit SHA, image build date, \
         and crate version. Use when asked what version/build/release is deployed or \
         how old the running deployment is."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        Ok(ToolResult::success(
            crate::build_info::as_json(),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_build_info_fields() {
        let result = SystemVersionTool
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.success);
        let output = result.output;
        assert!(output.get("version").is_some());
        assert!(output.get("commit_sha").is_some());
        assert!(output.get("build_date").is_some());
    }
}
