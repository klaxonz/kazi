//! OpenAI-compatible provider (`/chat/completions`).
//!
//! Works with any endpoint that implements the protocol: OpenAI, DeepSeek,
//! Kimi (Moonshot), Qwen (DashScope compatibility mode), OpenRouter, and
//! Ollama's `/v1`. Streaming tool calls arrive as *fragments* keyed by an
//! `index` field — the arguments JSON is assembled incrementally here.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::sse::SseDecoder;
use super::types::{LlmRequest, LlmResponse, Role, StreamEvent, ToolCall, Usage};
use super::{LlmError, Provider, ProviderSettings};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Accumulator that stitches streamed `delta.tool_calls` fragments back into
/// complete, ordered tool calls.
#[derive(Default)]
struct CallFragments {
    id: Option<String>,
    name: String,
    arguments: String,
}

pub struct OpenAiProvider {
    http: reqwest::Client,
    base: String,
    model: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(http: reqwest::Client, settings: &ProviderSettings) -> Self {
        Self {
            http,
            base: settings
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            model: settings.model.clone(),
            api_key: settings.api_key.clone(),
        }
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base)
    }

    /// Internal messages ⇄ `chat.completions` message objects.
    /// Tool-call arguments are re-stringified here: the wire format carries
    /// them as a JSON *string*, not an object.
    fn wire_messages(messages: &[super::types::Message]) -> Vec<Value> {
        use super::types::Message;
        messages
            .iter()
            .map(|m| match m {
                Message { role: Role::System, content, .. } => json!({"role": "system", "content": content.as_deref().unwrap_or("")}),
                Message { role: Role::User, content, .. } => json!({"role": "user", "content": content.as_deref().unwrap_or("")}),
                Message { role: Role::Assistant, content, tool_calls, .. } => {
                    let mut v = json!({"role": "assistant"});
                    if let Some(c) = content {
                        v["content"] = Value::String(c.clone());
                    } else {
                        v["content"] = Value::Null;
                    }
                    if !tool_calls.is_empty() {
                        v["tool_calls"] = Value::Array(
                            tool_calls.iter().map(|tc| json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": tc.arguments.to_string()},
                            })).collect(),
                        );
                    }
                    v
                }
                Message { role: Role::Tool, tool_call_id, content, .. } => json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id.clone().unwrap_or_default(),
                    "content": content.as_deref().unwrap_or(""),
                }),
            })
            .collect()
    }

    fn wire_request(&self, req: &LlmRequest, stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "stream": stream,
            "messages": Self::wire_messages(&req.messages),
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools.iter().map(|t| json!({
                "type": "function",
                "function": {"name": t.name, "description": t.description, "parameters": t.parameters},
            })).collect::<Vec<_>>());
        }
        body
    }

    fn parse_call(tc: &Value) -> ToolCall {
        let raw_args = tc["function"]["arguments"].as_str().unwrap_or("");
        let arguments =
            serde_json::from_str(raw_args).unwrap_or_else(|_| Value::String(raw_args.to_string()));
        ToolCall {
            id: tc["id"].as_str().unwrap_or("call_0").to_string(),
            name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
            arguments,
        }
    }

    /// Parse the model-listing response (`{"data": [{"id": "..."}, ...]}`).
    /// Shared shape for OpenAI-compatible and Anthropic `/models` endpoints.
    pub(crate) fn parse_models(v: &Value) -> Vec<String> {
        v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Parse a non-streaming `chat.completions` response.
    fn parse_response(v: &Value) -> Result<LlmResponse, LlmError> {
        let choice = v["choices"]
            .as_array()
            .and_then(|c| c.first())
            .ok_or_else(|| {
                LlmError::Format(format!(
                    "no choices in response: {}",
                    serde_json::to_string(&v["error"]).unwrap_or_default()
                ))
            })?;
        let msg = &choice["message"];
        let content = msg["content"]
            .as_str()
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        let tool_calls = msg["tool_calls"]
            .as_array()
            .map(|a| a.iter().map(Self::parse_call).collect())
            .unwrap_or_default();
        let usage = Usage {
            input_tokens: v["usage"]["prompt_tokens"].as_u64(),
            output_tokens: v["usage"]["completion_tokens"].as_u64(),
        };
        Ok(LlmResponse {
            content,
            tool_calls,
            stop_reason: choice["finish_reason"].as_str().map(str::to_string),
            usage,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn describe(&self) -> String {
        format!("openai-compatible {} ({})", self.model, self.base)
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut rb = self
            .http
            .post(self.url())
            .json(&self.wire_request(&req, false));
        if let Some(k) = &self.api_key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::http(status, &body));
        }
        let v: Value = resp.json().await?;
        Self::parse_response(&v)
    }

    async fn complete_stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let mut rb = self
            .http
            .post(self.url())
            .json(&self.wire_request(&req, true));
        if let Some(k) = &self.api_key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::http(status, &body));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut content = String::new();
        let mut calls: BTreeMap<u64, CallFragments> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage = Usage::default();
        let mut done = false;

        // Consume HTTP chunks; each may complete several SSE events. The
        // mpsc sender may be dropped by the consumer (cancellation) — honor
        // it by aborting with an error instead of blocking on send().
        while !done {
            tokio::select! {
                chunk = stream.next() => match chunk {
                    Some(Ok(c)) => {
                        let chunk = String::from_utf8_lossy(&c);
                        for ev in decoder.push(&chunk) {
                            if ev.data.trim() == "[DONE]" { done = true; continue; }
                            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                            let delta = &v["choices"][0]["delta"];
                            if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                                content.push_str(t);
                                if tx.send(StreamEvent::TextDelta(t.to_string())).await.is_err() {
                                    return Err(LlmError::Stream("stream consumer dropped".into()));
                                }
                            }
                            if let Some(frag) = delta["tool_calls"].as_array() {
                                for tc in frag {
                                    let idx = tc["index"].as_u64().unwrap_or_default();
                                    let slot = calls.entry(idx).or_default();
                                    if let Some(id) = tc["id"].as_str().filter(|i| !i.is_empty()) {
                                        slot.id = Some(id.to_string());
                                    }
                                    if let Some(n) = tc["function"]["name"].as_str() {
                                        slot.name.push_str(n);
                                    }
                                    if let Some(a) = tc["function"]["arguments"].as_str() {
                                        slot.arguments.push_str(a);
                                    }
                                }
                            }
                            if let Some(f) = v["choices"][0]["finish_reason"].as_str() {
                                finish_reason = Some(f.to_string());
                            }
                            if let Some(u) = v["usage"].as_object() {
                                usage.input_tokens = u["prompt_tokens"].as_u64();
                                usage.output_tokens = u["completion_tokens"].as_u64();
                            }
                        }
                    }
                    Some(Err(e)) => return Err(LlmError::Network(e)),
                    None => break,
                },
            }
        }

        // Assemble the final response, ordered by fragment index.
        let tool_calls: Vec<ToolCall> = calls
            .into_iter()
            .enumerate()
            .map(|(i, (_, f))| ToolCall {
                id: f.id.clone().unwrap_or_else(|| ToolCall::synthetic_id(i)),
                name: f.name,
                arguments: serde_json::from_str(&f.arguments)
                    .unwrap_or_else(|_| Value::String(f.arguments.clone())),
            })
            .collect();

        if content.is_empty() && tool_calls.is_empty() {
            return Err(LlmError::Format(
                "stream produced no text and no tool calls".into(),
            ));
        }
        Ok(LlmResponse {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls,
            stop_reason: finish_reason,
            usage,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let mut rb = self.http.get(format!("{}/models", self.base));
        if let Some(k) = &self.api_key {
            rb = rb.bearer_auth(k);
        }
        let resp = rb.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::http(status, &body));
        }
        let v: Value = resp.json().await?;
        Ok(Self::parse_models(&v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::Message;

    #[test]
    fn wire_messages_shape() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("list files"),
            Message::assistant_with_calls(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "list_dir".into(),
                    arguments: json!({"path": "."}),
                }],
            ),
            Message::tool_result("c1", "src/", false),
        ];
        let wire = OpenAiProvider::wire_messages(&msgs);
        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[1]["content"], "list files");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(
            wire[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\".\"}"
        );
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "c1");
    }

    #[test]
    fn parse_response_fixture() {
        let v = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_x",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 34}
        });
        let r = OpenAiProvider::parse_response(&v).unwrap();
        assert!(r.has_tool_calls());
        assert_eq!(r.tool_calls[0].name, "read_file");
        assert_eq!(r.tool_calls[0].arguments["path"], "a.rs");
        assert_eq!(r.stop_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.usage.input_tokens, Some(12));
        assert!(r.content.is_none());
    }

    #[test]
    fn parse_args_bad_json_kept_as_string() {
        let v = json!({
            "choices": [{"message": {"tool_calls": [
                {"id": "c", "type": "function", "function": {"name": "t", "arguments": "{oops"}}
            ]}}]
        });
        let r = OpenAiProvider::parse_response(&v).unwrap();
        assert_eq!(
            r.tool_calls[0].arguments,
            serde_json::Value::String("{oops".to_string())
        );
    }

    #[test]
    fn parse_models_reads_ids_and_tolerates_junk() {
        let v = json!({"object": "list", "data": [
            {"id": "gpt-4o", "object": "model"},
            {"id": "gpt-4o-mini"},
            {"object": "model"}          // no id: skipped
        ]});
        assert_eq!(
            OpenAiProvider::parse_models(&v),
            vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
        );
        // missing data array → empty, never a panic
        assert!(OpenAiProvider::parse_models(&json!({"error": "nope"})).is_empty());
    }
}
