//! Ollama local provider.
//!
//! Ollama ships an OpenAI-compatible endpoint at `http://localhost:11434/v1`,
//! so this provider is a thin preset over [`super::openai::OpenAiProvider`]:
//! different default base URL and no API key. All protocol handling,
//! including streaming, is inherited.

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::openai::OpenAiProvider;
use super::types::{LlmRequest, LlmResponse, StreamEvent};
use super::{LlmError, Provider, ProviderSettings};

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

pub struct OllamaProvider {
    inner: OpenAiProvider,
}

impl OllamaProvider {
    pub fn new(http: reqwest::Client, settings: &ProviderSettings) -> Self {
        let settings = ProviderSettings {
            kind: settings.kind,
            model: settings.model.clone(),
            base_url: Some(
                settings
                    .base_url
                    .clone()
                    .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            ),
            api_key: None,
            tuning: settings.tuning.clone(),
        };
        Self {
            inner: OpenAiProvider::new(http, &settings),
        }
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn describe(&self) -> String {
        format!("ollama model at {}", DEFAULT_BASE_URL)
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        self.inner.complete(req).await
    }

    async fn complete_stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        self.inner.complete_stream(req, tx).await
    }

    /// Inherited from the OpenAI-compatible transport — Ollama serves
    /// `GET /v1/models` too (its `/api/tags` is an equivalent source).
    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        <OpenAiProvider as Provider>::list_models(&self.inner).await
    }
}
