//! Anthropic Claude provider (Messages API `/v1/messages`).
//!
//! Protocol differences that make this file necessary:
//! - `system` is a *top-level parameter*, not a message.
//! - Tool calls are `content` blocks (`tool_use`) inside an assistant message.
//! - Tool results are `tool_result` blocks — and per the API they must live in
//!   a **user** message. Our history appends one `Tool` message per executed
//!   call, so consecutive `Role::Tool` messages are merged into a single user
//!   message with one `tool_result` block each.
//! - `max_tokens` is a required request parameter (we default to 4096).

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::sse::SseDecoder;
use super::types::{LlmRequest, LlmResponse, Message, Role, StreamEvent, ToolCall, Usage};
use super::{LlmError, Provider, ProviderSettings};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2023-06-01";
/// Anthropic requires an explicit output budget; a plain default for chat tasks.
const MAX_TOKENS: u32 = 4096;

/// Per-`content_block` accumulator for streaming.
#[derive(Default)]
struct BlockAcc {
    kind: String,
    id: String,
    name: String,
    text: String,
    json: String,
}

pub struct AnthropicProvider {
    http: reqwest::Client,
    base: String,
    model: String,
    api_key: String,
    tuning: super::Tuning,
}

impl AnthropicProvider {
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
            api_key: settings.api_key.clone().unwrap_or_default(),
            tuning: settings.tuning.clone(),
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.base)
    }

    fn tools_wire(tools: &[super::types::ToolSpec]) -> Value {
        json!(tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters}))
            .collect::<Vec<_>>())
    }

    fn ensure_object_arguments(arguments: &Value) -> Value {
        match arguments {
            Value::Object(_) => arguments.clone(),
            // Guard against malformed calls: tool_use.input must be an object.
            Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({"raw": s})),
            Value::Null => json!({}),
            other => json!({"raw": other.to_string()}),
        }
    }

    /// Convert unified history → (`system`, `messages` array).
    /// - `System` messages collapse into the top-level `system` param.
    /// - Consecutive `Tool` messages merge into ONE user message of
    ///   `tool_result` blocks (the API rejects `tool_result` outside user).
    /// - A leading assistant message (possible with resumed sessions) is
    ///   preceded by a stub user turn to satisfy strict user/assistant
    ///   alternation.
    fn wire(messages: &[Message]) -> (String, Vec<Value>) {
        let mut system_parts: Vec<String> = Vec::new();
        let mut out: Vec<Value> = Vec::new();
        let mut i = 0;
        while i < messages.len() {
            let m = &messages[i];
            match m {
                Message {
                    role: Role::System,
                    content,
                    ..
                } => {
                    if let Some(c) = content {
                        if !c.is_empty() {
                            system_parts.push(c.clone());
                        }
                    }
                    i += 1;
                }
                Message {
                    role: Role::User,
                    content,
                    ..
                } => {
                    out.push(json!({
                        "role": "user",
                        "content": [{"type": "text", "text": content.as_deref().unwrap_or("")}],
                    }));
                    i += 1;
                }
                Message {
                    role: Role::Assistant,
                    ..
                } => {
                    if out.is_empty() {
                        out.push(json!({"role": "user", "content": [{"type": "text", "text": "(context)"}]}));
                    }
                    let mut blocks: Vec<Value> = Vec::new();
                    if let Some(t) = m.content.as_deref().filter(|t| !t.is_empty()) {
                        blocks.push(json!({"type": "text", "text": t}));
                    }
                    for tc in &m.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": Self::ensure_object_arguments(&tc.arguments),
                        }));
                    }
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": "(empty)"}));
                    }
                    out.push(json!({"role": "assistant", "content": blocks}));
                    i += 1;
                }
                Message {
                    role: Role::Tool, ..
                } => {
                    // Merge the whole run of tool results into one user msg.
                    let mut results: Vec<Value> = Vec::new();
                    while i < messages.len() && messages[i].role == Role::Tool {
                        let t = &messages[i];
                        let mut block = json!({
                            "type": "tool_result",
                            "tool_use_id": t.tool_call_id.clone().unwrap_or_default(),
                            "content": [{"type": "text", "text": t.text()}],
                        });
                        if t.is_error {
                            block["is_error"] = json!(true);
                        }
                        results.push(block);
                        i += 1;
                    }
                    if !results.is_empty() {
                        out.push(json!({"role": "user", "content": results}));
                    }
                }
            }
        }
        (system_parts.join("\n\n"), out)
    }

    /// System prompt → content blocks with one cache breakpoint.
    ///
    /// `[Context summary of dropped conversation turns]` marks the volatile
    /// tail that compaction appends; everything before it is byte-stable for
    /// the life of the session and is what the cache breakpoint covers. Public
    /// so the wire shape is testable without a network round trip.
    pub fn system_blocks_parts(system: &str) -> Vec<Value> {
        system_blocks(system)
    }

    fn wire_request(&self, req: &LlmRequest, stream: bool) -> Value {
        let (system, messages) = Self::wire(&req.messages);
        let mut body = json!({
            "model": self.model,
            // `/effort` sets this; without a preset the old constant applies so
            // every endpoint keeps getting a valid, conservative budget.
            "max_tokens": self.tuning.max_tokens.unwrap_or(MAX_TOKENS),
            "stream": stream,
            "messages": messages,
        });
        if let Some(t) = self.tuning.temperature {
            body["temperature"] = json!(t);
        }
        if !system.is_empty() {
            // Prompt caching: mark the STABLE part of the system prompt. The
            // compaction summary is appended to it after a /compact, and a
            // single block would change on every compaction, invalidating the
            // whole cached prefix. Splitting keeps the base prompt cached
            // across compactions; only the summary block is re-billed.
            body["system"] = json!(system_blocks(&system));
        }
        if !req.tools.is_empty() {
            let mut tools = Self::tools_wire(&req.tools);
            // cache the whole catalog: mark the LAST tool, which caches every
            // schema before it in one breakpoint (max 4 are allowed per call)
            if let Some(last) = tools.as_array_mut().and_then(|a| a.last_mut()) {
                last["cache_control"] = json!({"type": "ephemeral"});
            }
            body["tools"] = tools;
            body["tool_choice"] = json!({"type": "auto"});
        }
        body
    }

    fn parse_response(v: &Value) -> Result<LlmResponse, LlmError> {
        let content_arr = v["content"]
            .as_array()
            .ok_or_else(|| LlmError::Format(format!("missing content array: {}", v)))?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in content_arr {
            match block["type"].as_str().unwrap_or("") {
                "text" => {
                    if let Some(t) = block["text"].as_str() {
                        text.push_str(t);
                    }
                }
                "tool_use" => tool_calls.push(ToolCall {
                    id: block["id"].as_str().unwrap_or("tool_0").to_string(),
                    name: block["name"].as_str().unwrap_or("").to_string(),
                    arguments: block["input"].clone(),
                }),
                _ => {}
            }
        }
        Ok(LlmResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            stop_reason: v["stop_reason"].as_str().map(str::to_string),
            usage: Usage {
                input_tokens: v["usage"]["input_tokens"].as_u64(),
                output_tokens: v["usage"]["output_tokens"].as_u64(),
                // Anthropic reports cache traffic next to the token counts;
                // without these two numbers a cached round looks free.
                cache_read_tokens: v["usage"]["cache_read_input_tokens"].as_u64(),
                cache_write_tokens: v["usage"]["cache_creation_input_tokens"].as_u64(),
            },
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn describe(&self) -> String {
        format!("anthropic {} ({})", self.model, self.base)
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let resp = self
            .http
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&self.wire_request(&req, false))
            .send()
            .await?;
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
        let resp = self
            .http
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&self.wire_request(&req, true))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::http(status, &body));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        // content blocks arrive unordered by index; BTreeMap keeps them sorted.
        let mut blocks: std::collections::BTreeMap<u64, BlockAcc> = Default::default();
        let mut stop_reason: Option<String> = None;
        let mut usage = Usage::default();
        let mut stream_err: Option<String> = None;

        'outer: while stream_err.is_none() {
            tokio::select! {
                chunk = stream.next() => match chunk {
                    Some(Ok(c)) => {
                        let chunk = String::from_utf8_lossy(&c);
                        for ev in decoder.push(&chunk) {
                            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
                            match v["type"].as_str().unwrap_or("") {
                                "error" => {
                                    stream_err = Some(v["error"]["message"].as_str().unwrap_or("unknown provider error").to_string());
                                    continue;
                                }
                                "content_block_start" => {
                                    let idx = v["index"].as_u64().unwrap_or_default();
                                    let slot = blocks.entry(idx).or_default();
                                    let cb = &v["content_block"];
                                    slot.kind = cb["type"].as_str().unwrap_or("").to_string();
                                    if let Some(id) = cb["id"].as_str() { slot.id = id.to_string(); }
                                    if let Some(n) = cb["name"].as_str() { slot.name = n.to_string(); }
                                }
                                "content_block_delta" => {
                                    let idx = v["index"].as_u64().unwrap_or_default();
                                    let slot = blocks.entry(idx).or_default();
                                    match v["delta"]["type"].as_str().unwrap_or("") {
                                        "text_delta" => {
                                            if let Some(t) = v["delta"]["text"].as_str() /* could be empty */ {
                                                slot.text.push_str(t);
                                                slot.kind = "text".into();
                                                if !t.is_empty() && tx.send(StreamEvent::TextDelta(t.to_string())).await.is_err() {
                                                    return Err(LlmError::Stream("stream consumer dropped".into()));
                                                }
                                            }
                                        }
                                        "input_json_delta" => {
                                            if let Some(s) = v["delta"]["partial_json"].as_str() {
                                                slot.json.push_str(s);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                "message_delta" => {
                                    if let Some(s) = v["delta"]["stop_reason"].as_str() {
                                        stop_reason = Some(s.to_string());
                                    }
                                    if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                                        usage.output_tokens = Some(o);
                                    }
                                }
                                "message_stop" => break 'outer,
                                _ => {} // message_start, content_block_stop, ping
                            }
                        }
                    }
                    Some(Err(e)) => return Err(LlmError::Network(e)),
                    None => break 'outer,
                },
            }
        }

        if let Some(e) = stream_err {
            return Err(LlmError::Stream(e));
        }

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for (_, b) in blocks {
            match b.kind.as_str() {
                "text" => text.push_str(&b.text),
                "tool_use" => tool_calls.push(ToolCall {
                    id: if b.id.is_empty() {
                        ToolCall::synthetic_id(tool_calls.len())
                    } else {
                        b.id.clone()
                    },
                    name: b.name,
                    arguments: serde_json::from_str(&b.json)
                        .unwrap_or_else(|_| Value::String(b.json.clone())),
                }),
                _ => {}
            }
        }
        if text.is_empty() && tool_calls.is_empty() && stream_err.is_none() && stop_reason.is_none()
        {
            return Err(LlmError::Format("stream produced no content".into()));
        }
        Ok(LlmResponse {
            content: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            stop_reason,
            usage,
        })
    }

    /// `GET /v1/models` — Anthropic lists models on the same `data[].id`
    /// envelope as OpenAI, so the parser is shared with [`super::openai`].
    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let resp = self
            .http
            .get(format!("{}/v1/models", self.base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::http(status, &body));
        }
        let v: Value = resp.json().await?;
        Ok(super::openai::OpenAiProvider::parse_models(&v))
    }
}

/// Split the system prompt at the compaction-summary marker: the base prompt
/// is cached, the summary tail rides along uncached.
fn system_blocks(system: &str) -> Vec<Value> {
    const MARK: &str = "[Context summary of dropped conversation turns]";
    match system.split_once(MARK) {
        Some((base, tail)) if !base.trim().is_empty() => vec![
            json!({
                "type": "text",
                "text": base.trim_end(),
                "cache_control": {"type": "ephemeral"},
            }),
            json!({"type": "text", "text": format!("{MARK}{tail}")}),
        ],
        _ => vec![json!({
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"},
        })],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(tuning: super::super::Tuning) -> AnthropicProvider {
        AnthropicProvider {
            http: reqwest::Client::new(),
            base: DEFAULT_BASE_URL.into(),
            model: "m".into(),
            api_key: "k".into(),
            tuning,
        }
    }

    #[test]
    fn cache_breakpoints_are_byte_stable() {
        // The cache only pays off if the marked prefix does not change. Two
        // compaction summaries appended at different times must leave the
        // base block byte-identical.
        let base = "You are kazi.\nWorking directory: D:/Code/init/kazi.";
        let with_a = format!("{base}\n\n[Context summary of dropped conversation turns]\n- did A");
        let with_b =
            format!("{base}\n\n[Context summary of dropped conversation turns]\n- did A\n- did B");
        let a = AnthropicProvider::system_blocks_parts(&with_a);
        let b = AnthropicProvider::system_blocks_parts(&with_b);
        assert_eq!(a.len(), 2, "base + summary tail");
        assert_eq!(a[0], b[0], "the cached block must not move");
        assert_ne!(a[1], b[1], "the tail is the volatile part");
        assert_eq!(a[0]["cache_control"]["type"], "ephemeral");

        // without a summary the whole prompt is the cached block
        let solo = AnthropicProvider::system_blocks_parts(base);
        assert_eq!(solo.len(), 1);
        assert_eq!(solo[0]["text"], base);
    }

    #[test]
    fn preset_reaches_the_wire() {
        let tuning = super::super::Tuning {
            temperature: Some(0.2),
            max_tokens: Some(8192),
            reasoning_effort: None,
        };
        let req = LlmRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
        };
        let body = provider(tuning).wire_request(&req, false);
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(
            body["max_tokens"], 8192,
            "a preset overrides the 4096 default"
        );
    }

    #[test]
    fn consecutive_tool_results_merge_into_one_user_message() {
        let msgs = vec![
            Message::user("run"),
            Message::assistant_with_calls(
                None,
                vec![
                    ToolCall {
                        id: "t1".into(),
                        name: "a".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "t2".into(),
                        name: "b".into(),
                        arguments: json!({}),
                    },
                ],
            ),
            Message::tool_result("t1", "ok", false),
            Message::tool_result("t2", "nope", true),
        ];
        let (system, wire) = AnthropicProvider::wire(&msgs);
        assert!(system.is_empty());
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[2]["role"], "user");
        let results = wire[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[1]["is_error"], true);
        assert_eq!(results[1]["tool_use_id"], "t2");
    }

    #[test]
    fn system_extracted_and_tools_use_input_schema() {
        let req = LlmRequest {
            messages: vec![Message::system("sit in one place"), Message::user("hi")],
            tools: vec![super::super::types::ToolSpec {
                name: "t".into(),
                description: "d".into(),
                parameters: json!({"type": "object", "properties": {}}),
            }],
        };
        let body = provider(super::super::Tuning::default()).wire_request(&req, false);
        // the system prompt is now a cache-marked block, not a bare string
        assert_eq!(body["system"][0]["text"], "sit in one place");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["system"].as_array().map(|a| a.len()) == Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        // the LAST tool carries the breakpoint, which caches the whole catalog
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert!(body["max_tokens"].is_u64());
        assert_eq!(body["stream"], false, "complete() must not stream");
    }

    #[test]
    fn streaming_request_sets_stream_true() {
        let req = LlmRequest {
            messages: vec![Message::user("hi")],
            tools: vec![],
        };
        let body = provider(super::super::Tuning::default()).wire_request(&req, true);
        assert_eq!(
            body["stream"], true,
            "complete_stream() must ask for SSE, else the decoder sees one JSON object"
        );
    }

    #[test]
    fn parse_response_mixed_blocks() {
        let v = json!({
            "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "tu1", "name": "list_dir", "input": {"path": "."}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 20}
        });
        let r = AnthropicProvider::parse_response(&v).unwrap();
        assert_eq!(r.content.as_deref(), Some("checking"));
        assert_eq!(r.tool_calls[0].id, "tu1");
        assert_eq!(r.tool_calls[0].arguments["path"], ".");
        assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn alternation_guard_prepends_stopper() {
        let msgs = vec![Message::assistant("only this")];
        let (_, wire) = AnthropicProvider::wire(&msgs);
        assert_eq!(wire.len(), 2);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[1]["role"], "assistant");
    }
}
