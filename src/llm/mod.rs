//! Provider abstraction: one async interface, three hand-rolled protocol
//! implementations. No agent framework, no SDK — just HTTP and JSON.

pub mod anthropic;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod sse;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

pub use types::{LlmRequest, LlmResponse, Message, Role, StreamEvent, ToolCall, ToolSpec, Usage};

/// Which wire protocol a configured provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Any OpenAI-compatible `/chat/completions` endpoint (OpenAI, DeepSeek,
    /// Kimi, Qwen, OpenRouter, ...).
    OpenAiCompat,
    /// Anthropic Claude Messages API (`/v1/messages`).
    Anthropic,
    /// Local Ollama served through its OpenAI-compatible `/v1` endpoint.
    /// Same protocol as [`ProviderKind::OpenAiCompat`], but base URL and
    /// key-free semantics are preset.
    Ollama,
}

/// Fully-resolved settings for one provider (from config, after env lookup).
#[derive(Debug, Clone)]
pub struct ProviderSettings {
    pub kind: ProviderKind,
    pub model: String,
    /// Base URL override. Defaults: OpenAI-compat `https://api.openai.com/v1`,
    /// Anthropic `https://api.anthropic.com`.
    pub base_url: Option<String>,
    /// Bearer key for OpenAI-compat / `x-api-key` for Anthropic.
    /// `None` is only valid for Ollama-style local endpoints.
    pub api_key: Option<String>,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("authentication failed (HTTP {0}): the API key is missing or invalid — check the provider's `api_key_env` variable in your config")]
    Auth(String),
    #[error("rate limited (HTTP 429) — wait a moment and retry, or lower `max_rounds`")]
    RateLimited,
    #[error("provider returned HTTP {status}: {body}")]
    Api { status: u16, body: String },
    #[error("provider response could not be parsed: {0}")]
    Format(String),
    #[error("stream ended unexpectedly: {0}")]
    Stream(String),
}

impl LlmError {
    /// Categorize a failed HTTP response by its status code, including the
    /// provider's error body for diagnosis.
    pub fn http(status: u16, body: &str) -> Self {
        match status {
            401 | 403 => Self::Auth(status.to_string()),
            429 => Self::RateLimited,
            _ => Self::Api {
                status,
                body: body.chars().take(2000).collect(),
            },
        }
    }
}

/// One model round against an LLM. All methods are per-call and stateless:
/// the conversation lives in the caller (the agent loop).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable, safe-to-echo description (key is never shown).
    fn describe(&self) -> String;

    /// Complete a round without streaming.
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError>;

    /// Complete a round while streaming progress into `tx`. Resolves with the
    /// final response *after* all events were sent. If the consumer drops `tx`,
    /// the implementation may abort with
    /// [`LlmError::Stream`] — cancellation is allowed here.
    async fn complete_stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError>;
}

/// Build an HTTP client shared by all providers.
/// No total request timeout (streamed rounds legitimately run for minutes);
/// `connect_timeout` still prevents hangs on unreachable hosts.
pub fn http_client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .user_agent(concat!("kazi/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(LlmError::Network)
}

/// Build the concrete provider matching `settings`.
pub fn build_provider(settings: &ProviderSettings) -> Result<Arc<dyn Provider>, LlmError> {
    let http = http_client()?;
    match settings.kind {
        ProviderKind::OpenAiCompat => Ok(Arc::new(openai::OpenAiProvider::new(http, settings))),
        ProviderKind::Anthropic => Ok(Arc::new(anthropic::AnthropicProvider::new(http, settings))),
        ProviderKind::Ollama => Ok(Arc::new(ollama::OllamaProvider::new(http, settings))),
    }
}
