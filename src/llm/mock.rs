//! Scripted provider for tests and `--provider mock` smoke runs.
//!
//! It simply yields the pre-programmed [`LlmResponse`]s in order. Once the
//! script is exhausted it repeats the last response (or an error if empty).
//! This makes deterministic agent-loop integration tests possible without
//! network access or API keys.

use std::collections::VecDeque;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use super::types::{LlmRequest, LlmResponse, StreamEvent};
use super::{LlmError, Provider};

pub struct MockProvider {
    script: Mutex<VecDeque<LlmResponse>>,
}

impl MockProvider {
    pub fn new(steps: Vec<LlmResponse>) -> Self {
        Self {
            script: Mutex::new(VecDeque::from(steps)),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn describe(&self) -> String {
        "mock (scripted, no network)".to_string()
    }

    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut q = self.script.lock().await;
        match q.pop_front() {
            Some(r) => Ok(r),
            None => Err(LlmError::Format("mock script exhausted".into())),
        }
    }

    async fn complete_stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let resp = self.complete(req).await?;
        if let Some(c) = &resp.content {
            let _ = tx.send(StreamEvent::TextDelta(c.clone())).await;
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::{LlmRequest, Message};

    fn req() -> LlmRequest {
        LlmRequest {
            messages: vec![Message::user("x")],
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn pops_steps_in_order_then_errors() {
        let p = MockProvider::new(vec![
            LlmResponse {
                content: Some("a".into()),
                ..Default::default()
            },
            LlmResponse {
                content: Some("b".into()),
                ..Default::default()
            },
        ]);
        assert_eq!(p.complete(req()).await.unwrap().content, Some("a".into()));
        assert_eq!(p.complete(req()).await.unwrap().content, Some("b".into()));
        assert!(p.complete(req()).await.is_err());
    }
}
