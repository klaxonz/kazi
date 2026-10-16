//! Minimal MCP (Model Context Protocol) stdio client, hand-rolled on
//! JSON-RPC 2.0 / newline-delimited messages — the same subset of the spec
//! that every MCP stdio server speaks:
//!
//! ```text
//! → initialize {protocolVersion, capabilities, clientInfo}
//! ← result {protocolVersion, capabilities, serverInfo}
//! → notification: notifications/initialized          (no id, no reply)
//! → tools/list  { }                                   ← result {tools: [...]}
//! → tools/call   {name, arguments}                    ← result {content, isError}
//! ← server → client request: ping                    → result { }
//! ```
//!
//! Transport is injected (`AsyncRead` + `AsyncWrite` halves): the real path
//! spawns a server process and hands in its stdio; tests use an in-process
//! duplex — that keeps the protocol logic testable without any executable.
//! Requests may be in flight concurrently (routed by JSON-RPC id); the
//! agent loop's `max_parallel_tools` cap bounds how many actually happen.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{oneshot, Mutex};
use tokio::time::Duration;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("cannot spawn server: {0}")]
    Spawn(String),
    #[error("mcp protocol: {0}")]
    Protocol(String),
    #[error("mcp server error {code}: {message}")]
    Server { code: i64, message: String },
    #[error("mcp call timed out after {0}s")]
    Timeout(u64),
    #[error("mcp io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp http transport: {0}")]
    Http(#[from] reqwest::Error),
}

/// Result of a `tools/call` as the server framed it: text plus the error flag.
#[derive(Debug, Clone)]
pub struct ToolCallOut {
    pub text: String,
    pub is_error: bool,
}

/// Pending request registry: id → queued resolution channel.
type PendingMap = Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;
/// Boxed outbound half of the transport.
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

pub struct McpClient {
    id: AtomicU64,
    write: Mutex<Writer>,
    pending: PendingMap,
    call_timeout: Duration,
}

impl McpClient {
    /// Wire a client over any byte transport and start its reader loop.
    pub fn over(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
        call_timeout: Duration,
    ) -> Arc<Self> {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let client = Arc::new(Self {
            id: AtomicU64::new(1),
            write: Mutex::new(Box::new(write)),
            pending: pending.clone(),
            call_timeout,
        });
        tokio::spawn(reader_loop(BufReader::new(read), pending, client.clone()));
        client
    }

    /// MCP handshake: initialize + notifications/initialized.
    pub async fn initialize(&self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "kazi", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        if result.get("serverInfo").is_none() && result.get("protocolVersion").is_none() {
            return Err(McpError::Protocol(
                "initialize returned no serverInfo — not an MCP server?".into(),
            ));
        }
        // session established; the notification carries no id → no reply
        self.notify("notifications/initialized").await;
        Ok(())
    }

    /// Tools this server exposes: (raw name, description, input schema).
    pub async fn list_tools(&self) -> Result<Vec<(String, String, Value)>, McpError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result["tools"]
            .as_array()
            .ok_or_else(|| McpError::Protocol("tools/list returned no tools array".into()))?;
        Ok(tools
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap_or("").to_string(),
                    t["description"].as_str().unwrap_or("").to_string(),
                    t.get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                )
            })
            .collect())
    }

    /// Run a tool on the server; text content blocks are joined with `\n`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallOut, McpError> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let mut text = String::new();
        if let Some(blocks) = result["content"].as_array() {
            for block in blocks {
                if block["type"].as_str() == Some("text") {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(block["text"].as_str().unwrap_or(""));
                }
            }
        }
        Ok(ToolCallOut { text, is_error })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send_raw(&line.to_string()).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_dropped)) => Err(McpError::Protocol("client shutting down".into())),
            Err(_elapsed) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout(self.call_timeout.as_secs()))
            }
        }
    }

    async fn notify(&self, method: &str) {
        let line = json!({"jsonrpc": "2.0", "method": method});
        let _ = self.send_raw(&line.to_string()).await;
    }

    async fn send_raw(&self, line: &str) -> Result<(), McpError> {
        let mut w = self.write.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    async fn reply_ping(&self, id: Value) {
        let line = json!({"jsonrpc": "2.0", "id": id, "result": {}});
        let _ = self.send_raw(&line.to_string()).await;
    }
}

/// Background reader: routes responses to their pending calls, answers
/// server-initiated pings, ignores notifications.
async fn reader_loop<R: AsyncRead + Unpin + Send>(
    mut lines: BufReader<R>,
    pending: PendingMap,
    client: Arc<McpClient>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(0) | Err(_) => return, // server closed / stream dead
            Ok(_) => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue; // tolerate keepalive junk lines
        };
        let method = v["method"].as_str();
        let id = v.get("id").and_then(Value::as_u64);

        if method == Some("ping") {
            client.reply_ping(v["id"].clone()).await;
            continue;
        }
        // Responses must carry an id that a pending call is waiting on.
        let Some(id) = id else { continue }; // notification → ignore
        if let Some(res) = v.get("result") {
            if let Some(tx) = pending.lock().await.remove(&id) {
                let _ = tx.send(Ok(res.clone()));
            }
        } else if let Some(err) = v.get("error") {
            let mapped = McpError::Server {
                code: err["code"].as_i64().unwrap_or(-1),
                message: err["message"].as_str().unwrap_or("unknown").to_string(),
            };
            if let Some(tx) = pending.lock().await.remove(&id) {
                let _ = tx.send(Err(mapped));
            }
        }
    }
}
