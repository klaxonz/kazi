//! MCP (Model Context Protocol) client: two transports, one session trait.
//!
//! - **stdio** (`client.rs`): subprocess pipes, hand-rolled JSON-RPC 2.0 over
//!   newline-delimited messages — the subset every MCP stdio server speaks.
//! - **streamable HTTP** (`http.rs`): remote servers; POST per message,
//!   plain-JSON or SSE responses, `Mcp-Session-Id` + Bearer auth.
//!
//! Configured servers load at agent build time; a server that fails to start
//! or initialize degrades to a terminal warning and its tools are skipped
//! (a dead MCP server never kills the agent).

pub mod bridge;
pub mod client;
pub mod http;

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::process::{Child, Command};

use async_trait::async_trait;
use serde_json::Value;

pub use bridge::McpTool;
pub use client::{McpClient, McpError, ToolCallOut};
pub use http::HttpMcpSession;

/// Per-call ceiling for `tools/call` (server-side work can be slow).
pub const MCP_CALL_TIMEOUT_SECS: u64 = 60;

/// One established MCP session, regardless of transport. stdio (subprocess
/// pipes) and Streamable HTTP (remote servers) both implement this — the
/// tool bridge and the loader only see these three calls.
#[async_trait]
pub trait McpSession: Send + Sync {
    async fn initialize(&self) -> Result<(), McpError>;
    async fn list_tools(&self) -> Result<Vec<(String, String, Value)>, McpError>;
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallOut, McpError>;
}

#[async_trait]
impl McpSession for McpClient {
    async fn initialize(&self) -> Result<(), McpError> {
        McpClient::initialize(self).await
    }
    async fn list_tools(&self) -> Result<Vec<(String, String, Value)>, McpError> {
        McpClient::list_tools(self).await
    }
    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallOut, McpError> {
        McpClient::call_tool(self, name, arguments).await
    }
}

/// Spawn every configured server and collect their bridged tools.
pub async fn load_mcp_tools(
    cfg: &crate::config::McpCfg,
    call_timeout: Duration,
) -> Vec<Arc<dyn crate::tools::Tool>> {
    let mut out: Vec<Arc<dyn crate::tools::Tool>> = Vec::new();
    for (nick, entry) in &cfg.servers {
        match load_one(nick, entry, call_timeout).await {
            Ok(tools) => out.extend(tools),
            Err(e) => eprintln!("[mcp] server {nick:?} skipped: {e}"),
        }
    }
    out
}

async fn load_one(
    nick: &str,
    entry: &crate::config::McpServerEntry,
    call_timeout: Duration,
) -> Result<Vec<Arc<dyn crate::tools::Tool>>, McpError> {
    // Transport selection: `url` → streamable HTTP; otherwise a local
    // subprocess is spawned over stdio. Both produce the same `McpSession`.
    let (session, child): (Arc<dyn McpSession>, Option<Arc<Child>>) = if let Some(url) = &entry.url
    {
        let key = match &entry.api_key_env {
            Some(env) => Some(
                std::env::var(env)
                    .map_err(|_| McpError::Spawn(format!("auth env var {env} is not set")))?,
            ),
            None => None,
        };
        (Arc::new(HttpMcpSession::new(url, key, call_timeout)), None)
    } else {
        let (c, ch) = spawn_stdio(entry, call_timeout).await?;
        (c, Some(ch))
    };

    session.initialize().await?;
    let mut out: Vec<Arc<dyn crate::tools::Tool>> = Vec::new();
    for (raw, description, schema) in session.list_tools().await? {
        out.push(Arc::new(McpTool {
            name: McpTool::exposed_name(nick, &raw),
            raw_name: raw,
            description,
            schema,
            client: session.clone(),
            child: child.clone(),
        }));
    }
    Ok(out)
}

/// Spawn a local MCP server process and connect it over stdio.
/// The returned child stays alive exactly as long as the caller keeps it.
async fn spawn_stdio(
    entry: &crate::config::McpServerEntry,
    call_timeout: Duration,
) -> Result<(Arc<McpClient>, Arc<Child>), McpError> {
    let command = entry
        .command
        .as_deref()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| McpError::Spawn("no `command` and no `url` configured".into()))?;
    let mut cmd = Command::new(command);
    cmd.args(&entry.args)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in &entry.env {
        cmd.env(k, v);
    }
    no_console_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| McpError::Spawn(format!("`{command}`: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| McpError::Spawn("no stdout pipe".into()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| McpError::Spawn("no stdin pipe".into()))?;
    // server stderr is log noise; drain it so a chatty server cannot block
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(drain(stderr));
    }
    let child = Arc::new(child);
    Ok((McpClient::over(stdout, stdin, call_timeout), child.clone()))
}

async fn drain<R: AsyncRead + Unpin + Send>(mut r: R) {
    let _ = tokio::io::copy(&mut r, &mut tokio::io::sink()).await;
}

/// CREATE_NO_WINDOW avoids console flashes for spawned servers.
#[cfg(windows)]
fn no_console_window(c: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // tokio Command exposes creation_flags natively on Windows
    let _ = c.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_c: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{AutoApprover, Tool, ToolCtx};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Minimal MCP echo server over a duplex stream (in-process, no exec).
    /// Plus a `slow`/`fast` pair that proves concurrent requests are routed
    /// by id, not by reply order: `slow` defers, `fast` replies immediately,
    /// and only then does the deferred one go out.
    async fn echo_server(stream: tokio::io::DuplexStream) {
        let (read, mut write) = tokio::io::split(stream);
        let mut lines = BufReader::new(read).lines();
        let mut deferred: Option<serde_json::Value> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let mut replies: Vec<serde_json::Value> = Vec::new();
            let reply = match v["method"].as_str() {
                Some("initialize") => json!({
                    "jsonrpc": "2.0", "id": v["id"].clone(),
                    "result": {
                        "protocolVersion": "2025-06-18",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "echo", "version": "0.0.1"}
                    }
                }),
                Some("tools/list") => json!({
                    "jsonrpc": "2.0", "id": v["id"].clone(),
                    "result": {"tools": [
                        {"name": "echo", "description": "Echo a message", "inputSchema": {"type": "object", "properties": {}}},
                        {"name": "fail", "description": "Always errors", "inputSchema": {"type": "object", "properties": {}}},
                        {"name": "hang", "description": "Never replies", "inputSchema": {"type": "object", "properties": {}}}
                    ]}
                }),
                Some("tools/call") => match v["params"]["name"].as_str() {
                    Some("echo") => json!({
                        "jsonrpc": "2.0", "id": v["id"].clone(),
                        "result": {"content": [{"type": "text", "text": format!("echo:{}", v["params"]["arguments"]["msg"].as_str().unwrap_or("?"))}]}
                    }),
                    Some("fail") => json!({
                        "jsonrpc": "2.0", "id": v["id"].clone(),
                        "result": {"content": [{"type": "text", "text": "boom"}], "isError": true}
                    }),
                    // deferred: answered after the NEXT call arrives
                    Some("slow") => {
                        deferred = Some(json!({
                            "jsonrpc": "2.0", "id": v["id"].clone(),
                            "result": {"content": [{"type": "text", "text": "echo:deferred"}]}
                        }));
                        continue;
                    }
                    Some("fast") => {
                        if let Some(d) = deferred.take() {
                            replies.push(d);
                        }
                        json!({
                            "jsonrpc": "2.0", "id": v["id"].clone(),
                            "result": {"content": [{"type": "text", "text": "echo:immediate"}]}
                        })
                    }
                    _ => continue, // "hang": no reply ever
                },
                Some("ping") => json!({"jsonrpc": "2.0", "id": v["id"].clone(), "result": {}}),
                _ => continue,
            };
            replies.push(reply);
            for r in replies {
                write.write_all(r.to_string().as_bytes()).await.unwrap();
                write.write_all(b"\n").await.unwrap();
                write.flush().await.unwrap();
            }
        }
    }

    fn connect() -> Arc<McpClient> {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_side);
        let client = McpClient::over(cr, cw, Duration::from_millis(500));
        tokio::spawn(echo_server(server_side));
        client
    }

    fn tool_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: Duration::from_secs(5),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(AutoApprover),
            checkpointing: None,
            todos: None,
            permission: crate::tools::Permission::default(),
            subagent: None,
        }
    }

    #[tokio::test]
    async fn full_chain_over_duplex() {
        let client = connect();
        client.initialize().await.expect("handshake");
        let tools = client.list_tools().await.expect("tools/list");
        assert_eq!(tools[0].0, "echo");
        assert_eq!(tools[0].1, "Echo a message");

        let out = client
            .call_tool("echo", json!({"msg": "hi"}))
            .await
            .expect("call");
        assert_eq!(out.text, "echo:hi");
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn concurrent_calls_match_by_id() {
        let client = connect();
        client.initialize().await.expect("handshake");
        // two requests in flight; the server answers `fast` BEFORE `slow`
        let (slow, fast) = tokio::join!(
            client.call_tool("slow", json!({})),
            client.call_tool("fast", json!({})),
        );
        let slow = slow.expect("deferred call resolves by id");
        let fast = fast.expect("immediate call resolves by id");
        assert_eq!(slow.text, "echo:deferred");
        assert_eq!(fast.text, "echo:immediate");
        // neither response leaked across
        assert!(!slow.is_error && !fast.is_error);
    }

    #[tokio::test]
    async fn is_error_maps_to_tool_failure() {
        let client = connect();
        client.initialize().await.unwrap();
        let tool = McpTool {
            name: "mcp_echo_fail".into(),
            raw_name: "fail".into(),
            description: "Always errors".into(),
            schema: json!({"type": "object"}),
            client,
            child: None,
        };
        let err = tool
            .run(&json!({}), &tool_ctx())
            .await
            .expect_err("isError must surface as failure");
        assert!(err.0.contains("reported error"), "got: {err}");
        assert!(err.0.contains("boom"));
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let client = connect();
        client.initialize().await.unwrap();
        let err = client
            .call_tool("hang", json!({}))
            .await
            .expect_err("hang must time out");
        assert!(matches!(err, McpError::Timeout(_)), "got: {err}");
    }

    #[tokio::test]
    async fn tool_names_and_spec_shapes() {
        let client = connect();
        client.initialize().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        let (raw, description, schema) = &tools[0];
        let tool = McpTool {
            name: McpTool::exposed_name("echo", raw),
            raw_name: raw.clone(),
            description: description.clone(),
            schema: schema.clone(),
            client,
            child: None,
        };
        assert_eq!(tool.name(), "mcp_echo_echo");
        let spec = tool.into_spec();
        assert_eq!(spec.name, "mcp_echo_echo");
        assert_eq!(spec.description, "Echo a message");
        assert_eq!(spec.parameters["type"], "object");
        let out = tool.run(&json!({"msg": "yo"}), &tool_ctx()).await.unwrap();
        assert_eq!(out, "echo:yo");
    }
}
