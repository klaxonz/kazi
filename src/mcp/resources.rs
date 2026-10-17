//! MCP resources and prompts as tools.
//!
//! Tools are the model's hands; resources are the server's *data* (files, rows,
//! documents) and prompts are its *templates*. Bridging both keeps a single
//! MCP server useful beyond "it exposes a function": the model can read the
//! resource a tool pointed at, and instantiate a prompt the server ships.
//!
//! Two tools, one concept each, rather than four: `mcp_resources` lists (no
//! `uri`) or reads (with `uri`); `mcp_prompts` lists (no `name`) or renders.
//! Both take the server nickname from `[mcp.servers.*]`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::McpRegistry;
use crate::tools::{arg_str, Tool, ToolCtx, ToolError};

/// Cap for one resource/prompt payload: a big document must not swallow the
/// context window in a single tool result.
const MAX_CHARS: usize = 64_000;

pub struct McpResources {
    pub registry: std::sync::Arc<McpRegistry>,
}

#[async_trait]
impl Tool for McpResources {
    fn name(&self) -> &str {
        "mcp_resources"
    }

    fn description(&self) -> &str {
        "List or read an MCP server's RESOURCES (files, rows, documents it exposes). Call with just `server` to list them, or add `uri` to read one. Servers without resource support say so. Read-only: no approval needed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "description": "MCP server nickname from config"},
                "uri": {"type": "string", "description": "Resource URI to read; omit to list"}
            },
            "required": ["server"]
        })
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let server = arg_str(args, "server")?;
        let session = self.registry.session(server).ok_or_else(|| {
            ToolError::new(format!(
                "no MCP server named {server:?}; configured: {}",
                self.registry.names().join(", ")
            ))
        })?;
        if let Some(uri) = args.get("uri").and_then(Value::as_str) {
            let text = session
                .read_resource(uri)
                .await
                .map_err(|e| ToolError::new(format!("resources/read failed: {e}")))?;
            return Ok(cap(text, uri));
        }
        let list = session
            .list_resources()
            .await
            .map_err(|e| ToolError::new(format!("resources/list failed: {e}")))?;
        if list.is_empty() {
            return Ok(format!("{server}: no resources"));
        }
        let mut out = format!("{server}: {} resource(s)\n", list.len());
        for r in list {
            out.push_str(&format!(
                "  {}\n    {}{}{}\n",
                r.uri,
                if r.name.is_empty() {
                    "(unnamed)"
                } else {
                    &r.name
                },
                r.mime_type
                    .as_deref()
                    .map(|m| format!(" [{m}]"))
                    .unwrap_or_default(),
                if r.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", r.description)
                }
            ));
        }
        out.push_str("  (mcp_resources with the uri reads one)");
        Ok(out)
    }
}

pub struct McpPrompts {
    pub registry: std::sync::Arc<McpRegistry>,
}

#[async_trait]
impl Tool for McpPrompts {
    fn name(&self) -> &str {
        "mcp_prompts"
    }

    fn description(&self) -> &str {
        "List or render an MCP server's PROMPT TEMPLATES. Call with just `server` to list them (name + arguments), or add `name` (and `arguments`) to render the prompt text. Servers without prompt support say so."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string", "description": "MCP server nickname from config"},
                "name": {"type": "string", "description": "Prompt template name; omit to list"},
                "arguments": {"type": "object", "description": "Template arguments (string values)"}
            },
            "required": ["server"]
        })
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let server = arg_str(args, "server")?;
        let session = self.registry.session(server).ok_or_else(|| {
            ToolError::new(format!(
                "no MCP server named {server:?}; configured: {}",
                self.registry.names().join(", ")
            ))
        })?;
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            let arguments = args.get("arguments").cloned().unwrap_or(json!({}));
            let text = session
                .get_prompt(name, arguments)
                .await
                .map_err(|e| ToolError::new(format!("prompts/get failed: {e}")))?;
            return Ok(cap(text, name));
        }
        let list = session
            .list_prompts()
            .await
            .map_err(|e| ToolError::new(format!("prompts/list failed: {e}")))?;
        if list.is_empty() {
            return Ok(format!("{server}: no prompts"));
        }
        let mut out = format!("{server}: {} prompt(s)\n", list.len());
        for p in list {
            out.push_str(&format!(
                "  {}{}{}\n",
                p.name,
                if p.arguments.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", p.arguments.join(", "))
                },
                if p.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", p.description)
                }
            ));
        }
        out.push_str("  (mcp_prompts with the name renders one)");
        Ok(out)
    }
}

/// Truncate oversized payloads with a visible note (never silent).
fn cap(text: String, label: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text;
    }
    format!(
        "{}\n…[{label} truncated at {MAX_CHARS} chars]",
        text.chars().take(MAX_CHARS).collect::<String>()
    )
}
