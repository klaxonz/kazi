//! The unified message model that every provider maps to and from.
//!
//! All providers (OpenAI-compatible, Anthropic, Ollama) speak different wire
//! protocols, but the agent loop only deals with these types. Each provider
//! implements a lossless translation `Message` ⇄ protocol format:
//!
//! | internal                | OpenAI-compat                    | Anthropic                          |
//! |-------------------------|----------------------------------|------------------------------------|
//! | `Role::System`          | `role:"system"` message          | top-level `system` param           |
//! | `Role::User`            | `role:"user"`                    | `{role:"user", content:[text]}`    |
//! | `Role::Assistant`+calls | `tool_calls:[{function:{...}}]`  | `content:[{type:"tool_use",...}]`  |
//! | `Role::Tool`            | `role:"tool"` + `tool_call_id`   | `{type:"tool_result",...}` (user)  |
//!
//! Tool-call arguments are always carried as a `serde_json::Value` (object).
//! If the model emitted malformed JSON, the raw string is kept so the agent
//! loop can surface the failure back to the model, which usually corrects
//! itself on the next round.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Result of a tool execution, tied to a specific [`ToolCall::id`].
    Tool,
}

/// One tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON object with the tool's arguments (or a raw string if unparseable).
    #[serde(default)]
    pub arguments: Value,
}

impl ToolCall {
    /// Yes, some wire formats drop the id — synthesize a stable one.
    pub fn synthetic_id(i: usize) -> String {
        format!("call_{i}")
    }
}

/// A single message in the conversation history.
///
/// Invariants maintained by the agent loop:
/// - `Role::Tool` always carries `tool_call_id` matching a preceding
///   `Role::Assistant` message's `tool_calls`.
/// - `Role::Assistant` may carry `content`, `tool_calls`, or both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For `Role::Tool`: whether the tool reported a failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
        }
    }
    /// Assistant message that requests tool calls; `text` may be empty.
    pub fn assistant_with_calls(text: Option<String>, calls: Vec<ToolCall>) -> Self {
        let content = text.filter(|t| !t.is_empty());
        Self {
            role: Role::Assistant,
            content,
            tool_calls: calls,
            tool_call_id: None,
            is_error: false,
        }
    }
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            is_error,
        }
    }
    /// Plain text content, or `""` when this message carries none.
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// A tool's self-description turned into a JSON-schema the model respects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON-Schema object: `{"type":"object","properties":{...},"required":[...]}`
    pub parameters: Value,
}

/// Everything one model round needs: the full history plus the tool catalog.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

/// Token usage as reported by the provider (absent fields stay `None`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    /// Accumulate another usage sample (absent counts add zero).
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = Some(self.input_tokens.unwrap_or(0) + other.input_tokens.unwrap_or(0));
        self.output_tokens =
            Some(self.output_tokens.unwrap_or(0) + other.output_tokens.unwrap_or(0));
    }
}

/// The outcome of one model round.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmResponse {
    /// Natural-language reply; empty when the model only calls tools.
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Provider-specific stop reason (`"stop"`, `"end_turn"`, `"tool_use"`, ...).
    pub stop_reason: Option<String>,
    pub usage: Usage,
}

impl LlmResponse {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
    pub fn text_or_empty(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }
}

/// Progressive updates while a provider streams a round.
///
/// Tool calls are *not* streamed as events: providers accumulate fragments
/// internally and return complete tool calls in the final [`LlmResponse`].
/// The UI learns about tool calls from the agent loop when execution starts.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A fragment of assistant text, in order.
    TextDelta(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(m: &Message) {
        let v = serde_json::to_value(m).unwrap();
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(&back, m);
    }

    #[test]
    fn message_roundtrip_variants() {
        roundtrip(&Message::system("be helpful"));
        roundtrip(&Message::user("hi"));
        roundtrip(&Message::assistant("doing"));
        roundtrip(&Message::tool_result("call_1", "42 things", true));
    }

    #[test]
    fn assistant_with_calls_roundtrip_and_shape() {
        let m = Message::assistant_with_calls(
            Some(String::new()),
            vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: json!({"path": "a.txt"}),
            }],
        );
        // empty text must be dropped, not stored as Some("")
        assert!(m.content.is_none());
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["tool_calls"][0]["arguments"]["path"], "a.txt");
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn assistant_with_calls_no_text_roundtrip() {
        let m = Message::assistant_with_calls(
            None,
            vec![ToolCall {
                id: "c2".into(),
                name: "x".into(),
                arguments: Value::Object(json!({}).as_object().unwrap().clone()),
            }],
        );
        let v = serde_json::to_value(&m).unwrap();
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }
}
