use std::sync::OnceLock;

use regex::Regex;

use crate::error::McpError;

/// Default per-server cap on the number of tools we'll register.
/// At 512, even a large first-party server (e.g. Harbor, ~257
/// tools and growing) fits with headroom, while a malicious server
/// still can't fill the agent's tool registry unbounded.
pub const DEFAULT_MAX_TOOLS_PER_SERVER: usize = 512;

/// Default safety bound on the number of *raw* tools a server may
/// advertise before we stop paging. Distinct from the registration cap:
/// a keep-filter (allowlist) reduces how many tools are *registered*, but
/// we still page through every raw tool to apply it, so a server that
/// pages without end needs a separate backstop. Generous (well above any
/// real server's count) — it's a DoS guard, not a curation knob.
pub const DEFAULT_MAX_RAW_TOOLS: usize = 16_384;

/// Default cap on a single tool description.
pub const DEFAULT_MAX_DESCRIPTION_BYTES: usize = 8 * 1024;

/// Default cap on a single tool's serialised input schema.
pub const DEFAULT_MAX_INPUT_SCHEMA_BYTES: usize = 64 * 1024;

/// Default cap on a single `tools/call` result body.
pub const DEFAULT_MAX_CONTENT_BYTES: usize = 1024 * 1024;

/// Schema-trust limits applied to every `tools/list` response and
/// every `tools/call` body. Centralising these means a malicious or
/// buggy MCP server can't blow up the agent's tool registry, fill
/// the LLM context with garbage, or burn unbounded memory on a
/// pathological response. Defaults are chosen to comfortably fit
/// every well-behaved real-world server we know of.
#[derive(Debug, Clone, Copy)]
pub struct McpLimits {
    pub max_tools_per_server: usize,
    pub max_raw_tools: usize,
    pub max_description_bytes: usize,
    pub max_input_schema_bytes: usize,
    pub max_content_bytes: usize,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            max_tools_per_server: DEFAULT_MAX_TOOLS_PER_SERVER,
            max_raw_tools: DEFAULT_MAX_RAW_TOOLS,
            max_description_bytes: DEFAULT_MAX_DESCRIPTION_BYTES,
            max_input_schema_bytes: DEFAULT_MAX_INPUT_SCHEMA_BYTES,
            max_content_bytes: DEFAULT_MAX_CONTENT_BYTES,
        }
    }
}

/// Tool-name pattern: ASCII letters, digits, dot, dash, underscore;
/// 1-64 chars. Tight enough to defend against shell-metacharacter
/// injection in display strings, loose enough for every real server.
fn tool_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,64}$").expect("static regex must compile"))
}

impl McpLimits {
    /// Validate a tool name. Returns `Err` with the offending name
    /// embedded so logs / errors point at the bad input.
    pub fn check_name(&self, name: &str) -> Result<(), McpError> {
        if tool_name_regex().is_match(name) {
            Ok(())
        } else {
            Err(McpError::SchemaViolation(format!(
                "tool name `{name}` violates `^[A-Za-z0-9._-]{{1,64}}$`"
            )))
        }
    }

    /// Validate a tool description.
    pub fn check_description(&self, name: &str, description: &str) -> Result<(), McpError> {
        if description.len() > self.max_description_bytes {
            return Err(McpError::SchemaViolation(format!(
                "tool `{name}` description is {} bytes, max {}",
                description.len(),
                self.max_description_bytes
            )));
        }
        Ok(())
    }

    /// Validate the *serialised* size of a tool's input schema.
    pub fn check_input_schema(
        &self,
        name: &str,
        schema: &serde_json::Value,
    ) -> Result<(), McpError> {
        let serialised = schema.to_string();
        if serialised.len() > self.max_input_schema_bytes {
            return Err(McpError::SchemaViolation(format!(
                "tool `{name}` inputSchema is {} bytes, max {}",
                serialised.len(),
                self.max_input_schema_bytes
            )));
        }
        Ok(())
    }

    /// Validate the count of tools to *register* (after any keep-filter).
    pub fn check_tool_count(&self, count: usize) -> Result<(), McpError> {
        if count > self.max_tools_per_server {
            return Err(McpError::ToolCountExceeded {
                kept: count,
                cap: self.max_tools_per_server,
            });
        }
        Ok(())
    }

    /// Validate the count of *raw* tools paged through so far. Bounds
    /// total work even when a keep-filter is discarding most tools.
    pub fn check_raw_count(&self, raw_count: usize) -> Result<(), McpError> {
        if raw_count > self.max_raw_tools {
            return Err(McpError::RawToolLimitExceeded {
                limit: self.max_raw_tools,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_accepts_typical_forms() {
        let l = McpLimits::default();
        for name in ["hello", "hello-world", "issue.create", "X9_test", "a"] {
            l.check_name(name).expect(name);
        }
    }

    #[test]
    fn name_rejects_shell_meta_and_oversize() {
        let l = McpLimits::default();
        for bad in [
            "; rm -rf /",
            "tool name",
            "../escape",
            "a/b",
            "",
            "$()",
            "👻",
        ] {
            assert!(l.check_name(bad).is_err(), "should reject `{bad}`");
        }
        let too_long = "a".repeat(65);
        assert!(l.check_name(&too_long).is_err());
    }

    #[test]
    fn description_size_enforced() {
        let l = McpLimits::default();
        l.check_description("ok", "short").unwrap();
        let oversize = "a".repeat(l.max_description_bytes + 1);
        assert!(l.check_description("bad", &oversize).is_err());
    }

    #[test]
    fn input_schema_size_enforced_after_serialisation() {
        let l = McpLimits {
            max_input_schema_bytes: 64,
            ..Default::default()
        };
        let small = serde_json::json!({"type": "object"});
        l.check_input_schema("ok", &small).unwrap();
        let large = serde_json::json!({"type": "object", "filler": "a".repeat(1000)});
        assert!(l.check_input_schema("bad", &large).is_err());
    }

    #[test]
    fn tool_count_cap_enforced() {
        let l = McpLimits::default();
        l.check_tool_count(0).unwrap();
        l.check_tool_count(DEFAULT_MAX_TOOLS_PER_SERVER).unwrap();
        assert!(l
            .check_tool_count(DEFAULT_MAX_TOOLS_PER_SERVER + 1)
            .is_err());
    }
}
