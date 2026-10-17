//! Provider abstraction: one async interface, three hand-rolled protocol
//! implementations. No agent framework, no SDK — just HTTP and JSON.

pub mod anthropic;
pub mod mock;
pub mod ollama;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod types;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

pub use retry::RetryProvider;
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
    /// Pre-formatted rejection text: the status plus whatever the provider
    /// said, so a plan/model refusal never reads as "your key is wrong".
    #[error("{0}")]
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
            401 | 403 => Self::Auth(format!(
                "request rejected by the provider (HTTP {status}): {}\n  → if that names a model or plan, the provider refused it; if it names a key, check the provider's `api_key_env`",
                provider_error_message(body)
            )),
            429 => Self::RateLimited,
            _ => Self::Api {
                status,
                body: body.chars().take(2000).collect(),
            },
        }
    }
}

/// Pull the provider's human-readable message out of an error body. Both the
/// OpenAI and Anthropic envelopes carry it at `error.message`; anything
/// unparseable is passed through (trimmed to a readable length).
pub fn provider_error_message(body: &str) -> String {
    const CAP: usize = 400;
    let text = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());
    if text.chars().count() > CAP {
        text.chars().take(CAP).collect::<String>() + "…"
    } else {
        text
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

    /// Model ids the endpoint reports as available, best-effort. Defaults to
    /// empty (a provider that cannot enumerate models); the picker then falls
    /// back to config/default lists. Never fatal — callers treat an error or an
    /// empty list the same way.
    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        Ok(Vec::new())
    }
}

/// Built-in fallback model list per provider kind, used when the endpoint
/// cannot be queried and config lists nothing. OpenAiCompat is intentionally
/// empty: gateway model names are arbitrary.
pub fn default_models(kind: ProviderKind) -> &'static [&'static str] {
    match kind {
        ProviderKind::Anthropic => &["claude-sonnet-4-5", "claude-opus-4-1", "claude-haiku-4-5"],
        ProviderKind::Ollama => &["qwen3:14b", "llama3.2", "deepseek-r1"],
        ProviderKind::OpenAiCompat => &[],
    }
}

/// Whether a model id can plausibly be served on this provider's wire. A
/// gateway's `/models` endpoint may list every model it proxies even though its
/// Anthropic wire (`/v1/messages`) only accepts Claude models — offering the
/// rest would just 400 on the next task (e.g. Command Code's Provider API:
/// "Use /provider/v1/chat/completions for OpenAI and OSS models"). The
/// OpenAI-compatible wire is left unfiltered because it legitimately carries
/// non-OpenAI ids too (OpenRouter serves `anthropic/claude-*`).
pub fn model_fits_wire(kind: ProviderKind, model: &str) -> bool {
    match kind {
        ProviderKind::Anthropic => model.to_ascii_lowercase().contains("claude"),
        ProviderKind::OpenAiCompat | ProviderKind::Ollama => true,
    }
}

/// Merge picker candidates: explicit config `models`, then the live endpoint
/// list, then the built-in fallback — deduped, with the current model hoisted
/// to the front when it is known. Model ids are case-sensitive. Live ids are
/// filtered to those the wire can actually serve ([`model_fits_wire`]); config
/// ids are taken as-is (an explicit user choice).
pub fn merge_catalog(
    config_models: &[String],
    live: &[String],
    kind: ProviderKind,
    current: Option<&str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(cur) = current.map(str::trim).filter(|c| !c.is_empty()) {
        out.push(cur.to_string());
        seen.insert(cur.to_string());
    }
    for m in config_models
        .iter()
        .map(String::as_str)
        .chain(
            live.iter()
                .map(String::as_str)
                .filter(|m| model_fits_wire(kind, m)),
        )
        .chain(default_models(kind).iter().copied())
    {
        let m = m.trim();
        if !m.is_empty() && seen.insert(m.to_string()) {
            out.push(m.to_string());
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejections_surface_the_provider_message() {
        // a plan gate must not read as "your key is wrong"
        let body = r#"{"type":"error","error":{"type":"permission_error","message":"MODEL_NOT_IN_PLAN: Claude Sonnet 4.6 available in Pro and above plans"}}"#;
        let msg = LlmError::http(403, body).to_string();
        assert!(msg.contains("MODEL_NOT_IN_PLAN"), "{msg}");
        assert!(msg.contains("HTTP 403"), "{msg}");

        // OpenAI-shaped envelope
        let body = r#"{"error":{"message":"invalid api key","type":"authentication_error"}}"#;
        assert!(LlmError::http(401, body)
            .to_string()
            .contains("invalid api key"));

        // unparseable body passes through, capped for the terminal
        let long = "x".repeat(1000);
        let msg = provider_error_message(&long);
        assert_eq!(msg.chars().count(), 401, "capped with an ellipsis");
    }

    #[test]
    fn merge_catalog_orders_dedups_and_hoists_current() {
        // OpenAiCompat wire keeps every live id (no wire filter)
        let cfg = vec!["cfg-a".to_string(), "shared".to_string()];
        let live = vec!["live-a".to_string(), "shared".to_string()];
        let merged = merge_catalog(&cfg, &live, ProviderKind::OpenAiCompat, Some("cur"));
        // current first, then config, then live
        assert_eq!(merged, vec!["cur", "cfg-a", "shared", "live-a"]);
    }

    #[test]
    fn merge_catalog_filters_live_models_by_wire() {
        // an Anthropic-wire gateway lists OSS models too, but only Claude ids
        // actually answer on /v1/messages — the rest must not be offered
        let live = vec![
            "deepseek/deepseek-v4.1-flash".to_string(),
            "claude-opus-4-1".to_string(),
        ];
        let merged = merge_catalog(&[], &live, ProviderKind::Anthropic, None);
        assert!(merged.contains(&"claude-opus-4-1".to_string()));
        assert!(
            !merged.iter().any(|m| m.contains("deepseek")),
            "OSS ids must be filtered off the Anthropic wire: {merged:?}"
        );
        // config ids are an explicit choice: never filtered
        let cfg = vec!["my-proxy-model".to_string()];
        let merged = merge_catalog(&cfg, &[], ProviderKind::Anthropic, None);
        assert!(merged.contains(&"my-proxy-model".to_string()));
    }

    #[test]
    fn merge_catalog_falls_back_to_defaults_and_handles_empty() {
        let merged = merge_catalog(&[], &[], ProviderKind::Ollama, None);
        assert!(merged.contains(&"qwen3:14b".to_string()));
        // OpenAiCompat has no defaults and no sources → empty
        assert!(merge_catalog(&[], &[], ProviderKind::OpenAiCompat, None).is_empty());
        // a current model alone is still a usable single-item list
        assert_eq!(
            merge_catalog(&[], &[], ProviderKind::OpenAiCompat, Some("my-model")),
            vec!["my-model".to_string()]
        );
    }
}
