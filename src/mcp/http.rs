//! Streamable-HTTP MCP transport (the 2025-06-18 distributed spec).
//!
//! One POST per JSON-RPC message to the server's MCP endpoint:
//! - the server answers with a plain JSON response, **or** an SSE stream that
//!   may contain the response plus later notifications/requests;
//! - responses are matched by their JSON-RPC `id`;
//! - a server can assign a session via `Mcp-Session-Id` (echoed back on every
//!   subsequent request);
//! - notifications get `202 Accepted` and are fire-and-forget;
//! - optional Bearer auth from an env var (keys never enter config files).
//!
//! Deliberate scope cut, documented honestly: no GET push stream (optional
//! per spec — without it the client simply does not receive unsolicited
//! server-to-client notifications; request/response flows stay complete).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use super::{McpError, McpSession, ToolCallOut};
use crate::llm::sse::SseDecoder;

/// What the server sent back for one POSTed message.
enum PostReply {
    /// `202 Accepted` — notification accepted, no body.
    Accepted,
    /// A JSON-RPC message carrying an `id` (response or server-side error).
    Response(Value),
}

pub struct HttpMcpSession {
    url: String,
    api_key: Option<String>,
    timeout: Duration,
    http: reqwest::Client,
    session: RwLock<Option<String>>,
    next_id: AtomicU64,
}

impl HttpMcpSession {
    pub fn new(url: &str, api_key: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kazi/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .expect("static client configuration");
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_key,
            timeout,
            http,
            session: RwLock::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// POST one message and classify what came back.
    async fn post_message(&self, msg: &Value) -> Result<PostReply, McpError> {
        let mut req = self
            .http
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(session) = self.session.read().await.clone() {
            req = req.header("Mcp-Session-Id", session);
        }
        let resp = req
            .timeout(self.timeout)
            .json(msg)
            .send()
            .await
            .map_err(|e| {
                // request-level timeout (headers never arrived) reads as Timeout
                if e.is_timeout() {
                    McpError::Timeout(self.timeout.as_secs())
                } else {
                    McpError::Http(e)
                }
            })?;

        // a session id may arrive on any response — capture it once
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            let sid = sid.to_str().unwrap_or("").to_string();
            if !sid.is_empty() {
                *self.session.write().await = Some(sid);
            }
        }

        let status = resp.status();
        if status == reqwest::StatusCode::ACCEPTED {
            return Ok(PostReply::Accepted);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(McpError::Server {
                code: status.as_u16() as i64,
                message: body.chars().take(500).collect(),
            });
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("event-stream") {
            self.drain_sse_for_response(resp).await
        } else {
            let v: Value = resp
                .json()
                .await
                .map_err(|e| McpError::Protocol(format!("non-JSON body: {e}")))?;
            if v.get("id").is_none() {
                // header-only notification riding as JSON — no result expected
                return Ok(PostReply::Accepted);
            }
            Ok(PostReply::Response(v))
        }
    }

    /// Scan an SSE response body for the JSON-RPC message that carries an id.
    /// Notifications are skipped; the stream ends when the reply shows up.
    async fn drain_sse_for_response(&self, resp: reqwest::Response) -> Result<PostReply, McpError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut stream = resp.bytes_stream();
        let mut dec = SseDecoder::new();
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => return Err(McpError::Timeout(self.timeout.as_secs())),
                Ok(Some(Err(e))) => return Err(McpError::Http(e)),
                Ok(Some(Ok(chunk))) => {
                    for ev in dec.push(&String::from_utf8_lossy(&chunk)) {
                        if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                            if v.get("id").is_some() {
                                return Ok(PostReply::Response(v));
                            }
                            // notification riding in the stream → keep scanning
                        }
                    }
                }
                Ok(None) => {
                    // stream closed without a response for this request
                    return Err(McpError::Protocol(
                        "SSE stream ended without a response".into(),
                    ));
                }
            }
        }
    }

    /// Fire a JSON-RPC request and unwrap its result/error.
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match self.post_message(&msg).await? {
            PostReply::Accepted => Err(McpError::Protocol(
                "expected a response, got notification-only 202".into(),
            )),
            PostReply::Response(v) => {
                if let Some(err) = v.get("error") {
                    Err(McpError::Server {
                        code: err["code"].as_i64().unwrap_or(-1),
                        message: err["message"].as_str().unwrap_or("unknown").to_string(),
                    })
                } else {
                    Ok(v["result"].clone())
                }
            }
        }
    }

    async fn send_notification(&self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        let _ = self.post_message(&msg).await;
    }
}

#[async_trait]
impl McpSession for HttpMcpSession {
    async fn initialize(&self) -> Result<(), McpError> {
        let result = self
            .rpc(
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
        self.send_notification("notifications/initialized").await;
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<(String, String, Value)>, McpError> {
        let result = self.rpc("tools/list", json!({})).await?;
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

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallOut, McpError> {
        let result = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// Tiny hand-rolled HTTP/1.1 MCP server used as the remote under test:
    /// it allocates session ids, records auth/session header usage, and can
    /// answer with plain JSON or a SSE stream, or hang (timeout coverage).
    struct MockCfg {
        sse: bool,
        hang: bool,
        auth_seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        session_seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    async fn spawn_mock(cfg: MockCfg) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let auth = cfg.auth_seen.clone();
        let session = cfg.session_seen.clone();
        let sse = cfg.sse;
        let hang = cfg.hang;
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    return;
                };
                let auth = auth.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(sock, auth, session, sse, hang).await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    async fn handle_conn(
        mut sock: TcpStream,
        auth_seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        session_seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        sse: bool,
        hang: bool,
    ) -> std::io::Result<()> {
        // read until the header/body boundary
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            let n = sock.read(&mut byte).await?;
            if n == 0 {
                return Ok(());
            }
            buf.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let mut content_length = 0usize;
        for line in head.lines() {
            let (key, value) = match line.split_once(':') {
                Some((k, v)) => (k.to_lowercase(), v.trim()),
                None => continue,
            };
            match key.as_str() {
                "authorization" => *auth_seen.lock().unwrap() = Some(value.to_string()),
                "mcp-session-id" => *session_seen.lock().unwrap() = Some(value.to_string()),
                "content-length" => content_length = value.parse().unwrap_or(0),
                _ => {}
            }
        }
        let mut body = vec![0u8; content_length];
        sock.read_exact(&mut body).await?;
        let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let id = req.get("id").cloned();

        if hang && req["method"] == "tools/call" {
            // only tool calls hang; the handshake must still succeed
            tokio::time::sleep(Duration::from_secs(5)).await;
            return Ok(()); // nothing sent → client should time out first
        }

        let result = match req["method"].as_str() {
            Some("initialize") => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "0"}
            }),
            Some("tools/list") => json!({"tools": [
                {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}},
                {"name": "fail", "description": "Fail", "inputSchema": {"type": "object"}}
            ]}),
            Some("tools/call") if req["params"]["name"] == "fail" => json!({
                "content": [{"type": "text", "text": "boom"}], "isError": true
            }),
            Some("tools/call") => json!({
                "content": [{"type": "text",
                             "text": format!("echo:{}", req["params"]["arguments"]["msg"].as_str().unwrap_or("?"))}]
            }),
            _ => return Ok(()), // notification → close; client treats 202 separately
        };

        let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
        let mut head_out = String::new();
        if sse {
            head_out.push_str(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n",
            );
        } else {
            head_out.push_str(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n",
            );
        }
        head_out.push_str("Mcp-Session-Id: sess-1\r\n");
        head_out.push_str("\r\n");
        sock.write_all(head_out.as_bytes()).await?;

        if sse {
            let payload = format!("data: {}\n\n", reply);
            sock.write_all(payload.as_bytes()).await?;
        } else {
            sock.write_all(reply.to_string().as_bytes()).await?;
        }
        sock.flush().await?;
        Ok(())
    }

    async fn client_on(cfg: MockCfg) -> HttpMcpSession {
        let url = spawn_mock(cfg).await;
        HttpMcpSession::new(&url, Some("tok".into()), Duration::from_millis(700))
    }

    #[tokio::test]
    async fn handshake_tools_call_over_json() {
        let auth = std::sync::Arc::new(std::sync::Mutex::new(None));
        let session = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = spawn_mock(MockCfg {
            sse: false,
            hang: false,
            auth_seen: auth.clone(),
            session_seen: session.clone(),
        })
        .await;
        let c = HttpMcpSession::new(&url, Some("tok".into()), Duration::from_millis(700));

        c.initialize().await.expect("handshake");
        let tools = c.list_tools().await.expect("list");
        assert_eq!(tools[0].0, "echo");
        let out = c.call_tool("echo", json!({"msg": "hi"})).await.unwrap();
        assert_eq!(out.text, "echo:hi");
        assert!(!out.is_error);

        assert_eq!(auth.lock().unwrap().as_deref(), Some("Bearer tok"));
        assert_eq!(session.lock().unwrap().as_deref(), Some("sess-1"));
    }

    #[tokio::test]
    async fn sse_streamed_response() {
        // the body arrives as an SSE stream instead of a plain JSON document —
        // the client must scan for its own response id.
        let c = client_on(MockCfg {
            sse: true,
            hang: false,
            auth_seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
            session_seen: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
        .await;
        c.initialize().await.expect("handshake over SSE");
        let out = c.call_tool("echo", json!({"msg": "sse"})).await.unwrap();
        assert_eq!(out.text, "echo:sse");
    }

    #[tokio::test]
    async fn server_error_surfaces() {
        let url = spawn_mock(MockCfg {
            sse: false,
            hang: false,
            auth_seen: Default::default(),
            session_seen: Default::default(),
        })
        .await;
        let c = HttpMcpSession::new(&url, None, Duration::from_millis(700));
        c.initialize().await.unwrap();
        let out = c.call_tool("fail", json!({})).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.text, "boom");
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let url = spawn_mock(MockCfg {
            sse: false,
            hang: true,
            auth_seen: Default::default(),
            session_seen: Default::default(),
        })
        .await;
        let c = HttpMcpSession::new(&url, None, Duration::from_millis(500));
        let err = c
            .call_tool("anything", json!({}))
            .await
            .expect_err("timeout");
        assert!(matches!(err, McpError::Timeout(_)), "got: {err}");
    }

    #[tokio::test]
    async fn auth_is_optional() {
        let auth_seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = spawn_mock(MockCfg {
            sse: false,
            hang: false,
            auth_seen: auth_seen.clone(),
            session_seen: Default::default(),
        })
        .await;
        let c = HttpMcpSession::new(&url, None, Duration::from_millis(700));
        c.initialize().await.unwrap();
        assert!(auth_seen.lock().unwrap().is_none(), "no auth header sent");
    }
}
