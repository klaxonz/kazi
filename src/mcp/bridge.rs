//! Bridge: MCP server tools become kazi tools.
//!
//! Naming: `mcp_<server>_<rawname>`, sanitized to the wire-legal charset
//! (`[A-Za-z0-9_-]`, ≤ 64 chars) so both provider protocols accept it.
//! Failure mapping: `isError: true` (or transport errors) become error
//! tool-results, which the model sees and can react to.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Child;

use crate::llm::ToolSpec;
use crate::tools::{Tool, ToolCtx, ToolError};

use super::client::{McpClient, ToolCallOut};

/// One tool served by one MCP server.
pub struct McpTool {
    /// sanitized exposed name (registry key).
    pub name: String,
    /// raw name to send back in `tools/call`.
    pub raw_name: String,
    pub description: String,
    pub schema: Value,
    pub client: Arc<McpClient>,
    /// The server process, kept alive for as long as any of its tools live
    /// (`kill_on_drop` reaps it when the registry is replaced). `None` only
    /// in tests that use an in-process transport.
    pub child: Option<Arc<Child>>,
}

impl McpTool {
    /// `mcp_<server>_<raw>`, charset- and length-cleaned.
    pub fn exposed_name(server: &str, raw: &str) -> String {
        let clean = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        let mut name = format!("mcp_{}_{}", clean(server), clean(raw));
        name.truncate(64);
        name
    }

    pub fn into_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let out: ToolCallOut = self
            .client
            .call_tool(&self.raw_name, args.clone())
            .await
            .map_err(|e| ToolError::new(e.to_string()))?;
        if out.is_error {
            return Err(ToolError::new(format!(
                "mcp tool reported error: {}",
                out.text
            )));
        }
        if out.text.is_empty() {
            return Ok("(tool ran without output)".into());
        }
        Ok(out.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_names_sanitized() {
        assert_eq!(
            McpTool::exposed_name("files", "read_file"),
            "mcp_files_read_file"
        );
        // legal cleansing: any non [A-Za-z0-9_-] becomes '_' before joining
        assert_eq!(
            McpTool::exposed_name("weird.server/x", "a.b-c d"),
            "mcp_weird_server_x_a_b-c_d"
        );
        // overlong raw names truncate to the 64-char wire limit
        let long = "x".repeat(100);
        assert_eq!(McpTool::exposed_name("s", &long).len(), 64);
    }
}
