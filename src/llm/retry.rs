//! Retry decorator for providers: handles the failures that are worth
//! retrying (rate limits, transient 5xx, network hiccups) with exponential
//! backoff, and never retries what would produce the same result again
//! (auth errors, malformed bodies, mid-stream aborts).
//!
//! Streaming retry safety: deltas may already have reached the UI before a
//! stream fails..retry is only allowed while **nothing** was sent — the
//! wrapper counts events forwarded to the caller and gives up retrying the
//! moment the first delta went out (the model's partial answer would have
//! been duplicated otherwise).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{LlmError, LlmRequest, LlmResponse, Provider, StreamEvent};

const BASE_DELAY: Duration = Duration::from_millis(800);
const MAX_DELAY: Duration = Duration::from_secs(30);

pub struct RetryProvider {
    inner: Arc<dyn Provider>,
    max_retries: u32,
}

impl RetryProvider {
    pub fn new(inner: Arc<dyn Provider>, max_retries: u32) -> Self {
        Self { inner, max_retries }
    }

    fn delay(attempt: u32) -> Duration {
        // 800ms, 1.6s, 3.2s ... capped at 30s
        let ms = BASE_DELAY.as_millis() as u64 * (1u64 << attempt.min(8));
        Duration::from_millis(ms).min(MAX_DELAY)
    }
}

fn retryable(err: &LlmError) -> bool {
    match err {
        LlmError::RateLimited => true,
        LlmError::Api { status, .. } => *status >= 500,
        // Network already covers reqwest transport errors
        LlmError::Network(_) => true,
        // auth / parse / mid-stream failures: retrying repeats the failure
        LlmError::Auth(_) | LlmError::Format(_) | LlmError::Stream(_) => false,
    }
}

#[async_trait]
impl Provider for RetryProvider {
    fn describe(&self) -> String {
        format!("{} (retry×{})", self.inner.describe(), self.max_retries)
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut attempt = 0u32;
        loop {
            match self.inner.complete(req.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(e) if retryable(&e) && attempt < self.max_retries => {
                    attempt += 1;
                    tokio::time::sleep(Self::delay(attempt - 1)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn complete_stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let mut attempt = 0u32;
        loop {
            // inner channel → forward to the caller, counting forwarded events
            let (inner_tx, mut inner_rx) = mpsc::channel::<StreamEvent>(64);
            let external_tx = tx.clone();
            let sent_any = Arc::new(AtomicBool::new(false));
            let forwarder_sent = sent_any.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = inner_rx.recv().await {
                    forwarder_sent.store(true, Ordering::Relaxed);
                    // a dropped consumer also stops the provider stream
                    if external_tx.send(ev).await.is_err() {
                        break;
                    }
                }
            });

            let result = self.inner.complete_stream(req.clone(), inner_tx).await;

            match result {
                Ok(resp) => {
                    // inner sender dropped by the provider: drain-then-join so
                    // every remaining event reached the caller before we return
                    let _ = forwarder.await;
                    return Ok(resp);
                }
                Err(e) => {
                    // provider dropped its sender on failure: the forwarder
                    // finishes on its own
                    let _ = forwarder.await;
                    if retryable(&e)
                        && !sent_any.load(Ordering::Relaxed)
                        && attempt < self.max_retries
                    {
                        attempt += 1;
                        tokio::time::sleep(Self::delay(attempt - 1)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Discovery is a single cheap GET: forward it straight through (no retry
    /// accounting; the picker tolerates an error by falling back to config).
    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::Message;
    use std::sync::atomic::AtomicUsize;

    fn req() -> LlmRequest {
        LlmRequest {
            messages: vec![Message::user("x")],
            tools: vec![],
        }
    }

    /// Fails the first N calls with `RateLimited`, then succeeds.
    struct FlakyOnce {
        fail_times: AtomicUsize,
    }

    impl FlakyOnce {
        fn new(fail_times: usize) -> Self {
            Self {
                fail_times: AtomicUsize::new(fail_times),
            }
        }
    }

    #[async_trait]
    impl Provider for FlakyOnce {
        fn describe(&self) -> String {
            "flaky".into()
        }
        async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
            let left = self.fail_times.fetch_sub(1, Ordering::Relaxed);
            if left > 0 {
                Err(LlmError::RateLimited)
            } else {
                Ok(LlmResponse {
                    content: Some("ok".into()),
                    ..Default::default()
                })
            }
        }
        async fn complete_stream(
            &self,
            req: LlmRequest,
            tx: mpsc::Sender<StreamEvent>,
        ) -> Result<LlmResponse, LlmError> {
            let resp = self.complete(req).await?;
            let _ = tx.send(StreamEvent::TextDelta("ok".into())).await;
            Ok(resp)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_success() {
        // fail twice, then succeed; retries must hide both rate limits
        let p: Arc<dyn Provider> = Arc::new(RetryProvider::new(Arc::new(FlakyOnce::new(2)), 3));
        let r = p.complete(req()).await.expect("retry should recover");
        assert_eq!(r.content.as_deref(), Some("ok"));
    }

    #[tokio::test(start_paused = true)]
    async fn budget_exhausted_returns_error() {
        let p: Arc<dyn Provider> = Arc::new(RetryProvider::new(
            Arc::new(FlakyOnce::new(10)), // fails forever
            2,
        ));
        let err = p.complete(req()).await.expect_err("still failing");
        assert!(matches!(err, LlmError::RateLimited));
    }

    #[tokio::test(start_paused = true)]
    async fn auth_failures_never_retry() {
        struct AuthFail;
        #[async_trait]
        impl Provider for AuthFail {
            fn describe(&self) -> String {
                "auth".into()
            }
            async fn complete(&self, _r: LlmRequest) -> Result<LlmResponse, LlmError> {
                Err(LlmError::Auth("401".into()))
            }
            async fn complete_stream(
                &self,
                _r: LlmRequest,
                _tx: mpsc::Sender<StreamEvent>,
            ) -> Result<LlmResponse, LlmError> {
                Err(LlmError::Auth("401".into()))
            }
        }
        let p: Arc<dyn Provider> = Arc::new(RetryProvider::new(Arc::new(AuthFail), 3));
        let err = p.complete(req()).await.expect_err("auth never recovers");
        assert!(matches!(err, LlmError::Auth(_)));
    }

    #[tokio::test(start_paused = true)]
    async fn streamed_retry_only_before_first_delta() {
        // emits one delta then fails → that failure is "mid-stream" and must
        // NOT be retried (a retry would duplicate the printed text)
        struct DeltaThenFail {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl Provider for DeltaThenFail {
            fn describe(&self) -> String {
                "mid".into()
            }
            async fn complete(&self, _r: LlmRequest) -> Result<LlmResponse, LlmError> {
                panic!("not used in this test")
            }
            async fn complete_stream(
                &self,
                _r: LlmRequest,
                tx: mpsc::Sender<StreamEvent>,
            ) -> Result<LlmResponse, LlmError> {
                let _ = self.calls.fetch_add(1, Ordering::Relaxed);
                tx.send(StreamEvent::TextDelta("partial".into())).await.ok();
                Err(LlmError::RateLimited) // after the delta → never retried
            }
        }
        let inner = Arc::new(DeltaThenFail {
            calls: AtomicUsize::new(0),
        });
        let p: Arc<dyn Provider> =
            Arc::new(RetryProvider::new(inner.clone() as Arc<dyn Provider>, 5));
        let (tx, mut rx) = mpsc::channel(16);
        let err = p
            .complete_stream(req(), tx)
            .await
            .expect_err("mid-stream fails");
        assert!(matches!(err, LlmError::RateLimited));
        while let Some(StreamEvent::TextDelta(_)) = rx.recv().await {}
        assert_eq!(
            inner.calls.load(Ordering::Relaxed),
            1,
            "no retry after a delta went out"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn streamed_retry_before_any_delta() {
        // fails before emitting anything (e.g. 429 on connect) → retried
        struct FailThenEmit {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl Provider for FailThenEmit {
            fn describe(&self) -> String {
                "flaky-stream".into()
            }
            async fn complete(&self, _r: LlmRequest) -> Result<LlmResponse, LlmError> {
                panic!("not used")
            }
            async fn complete_stream(
                &self,
                _r: LlmRequest,
                tx: mpsc::Sender<StreamEvent>,
            ) -> Result<LlmResponse, LlmError> {
                let n = self.calls.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    return Err(LlmError::RateLimited); // before any event
                }
                tx.send(StreamEvent::TextDelta("ok".into())).await.ok();
                Ok(LlmResponse {
                    content: Some("ok".into()),
                    ..Default::default()
                })
            }
        }
        let inner = Arc::new(FailThenEmit {
            calls: AtomicUsize::new(0),
        });
        let p: Arc<dyn Provider> =
            Arc::new(RetryProvider::new(inner.clone() as Arc<dyn Provider>, 3));
        let (tx, mut rx) = mpsc::channel(16);
        let resp = p
            .complete_stream(req(), tx)
            .await
            .expect("pre-delta retry recovers");
        assert_eq!(resp.content.as_deref(), Some("ok"));
        let mut got = String::new();
        while let Some(StreamEvent::TextDelta(t)) = rx.recv().await {
            got.push_str(&t);
        }
        assert_eq!(got, "ok", "exactly one delta must reach the consumer");
        assert_eq!(inner.calls.load(Ordering::Relaxed), 2);
    }
}
