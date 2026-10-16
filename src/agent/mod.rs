//! The agent engine: the loop that makes an LLM an agent.
//!
//! Per user task:
//!
//! ```text
//! loop (≤ max_rounds):
//!     response = provider.complete_stream(history + tool_catalog)   // text streams out
//!     no tool calls  → final answer, done
//!     tool calls     → execute each, append results to history, loop
//! ```
//!
//! Every tool result — including failures — goes back as a tool message, so
//! the model can self-correct on its next round instead of the loop dying.
//! Rounds are capped to guarantee termination.

pub mod context;
pub mod repl;
pub mod session;
pub mod tui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::llm::{
    LlmError, LlmRequest, LlmResponse, Message, Provider, StreamEvent, ToolCall, Usage,
};
use crate::tools::{ToolCtx, ToolOutcome, ToolRegistry};

pub const DEFAULT_MAX_ROUNDS: u32 = 30;
/// Rough chat-task budget; History trimming estimates 4 chars ≈ 1 token.
pub const DEFAULT_MAX_CONTEXT_TOKENS: u64 = 60_000;
/// Cap on in-one-round concurrent tool executions.
pub const DEFAULT_MAX_PARALLEL: usize = 4;

/// Approver selection: terminal prompt by default, blanket yes with `--yolo`.
fn approver(yolo: bool) -> Arc<dyn crate::tools::Approver> {
    if yolo {
        Arc::new(crate::tools::AutoApprover)
    } else {
        Arc::new(repl::StdinApprover::default())
    }
}

/// The default system prompt. Terse on purpose: the tools carry their own
/// contracts, the loop carries the mechanics; a wall of instructions here
/// would burn context. Mentioned behaviors: read before write, adapt on
/// failure, concrete reporting.
fn system_prompt(cwd: &std::path::Path) -> String {
    format!(
        "You are kazi, a focused assistant operating in the user's terminal.\n\
         Working directory: {}.\n\n\
         Ground rules:\n\
         - Be concrete: real paths, counts, exit codes. Never invent facts.\n\
         - Call tools one at a time and read each result before deciding the next step.\n\
         - Read files before modifying them. Prefer the file tools over running\n\
           commands for pure inspection.\n\
         - If a tool fails, adjust its inputs or take another approach — never\n\
           repeat a failing call unchanged.\n\
         - Answer in the user's language. Keep replies tight; no filler.",
        cwd.display()
    )
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] LlmError),
    #[error(
        "reached the {0}-round budget without a final answer — narrow the task, or raise agent.max_rounds in config"
    )]
    RoundsExceeded(u32),
    /// Stopped on request (Esc in the TUI / cancel token). History is kept.
    #[error("task cancelled")]
    Cancelled,
}

/// What the UI wants to know while a task runs.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Assistant text fragment, in order.
    TextDelta { text: String },
    /// A tool call is starting.
    ToolStart { name: String },
    /// Tool call finished: error flag + one-line preview of its output.
    ToolEnd {
        name: String,
        is_error: bool,
        preview: String,
    },
    /// Auto-compact kicking in between rounds (proactive, at 80% budget).
    AutoCompactStarted,
    /// Auto-compact finished: `note` describes what happened, `est_tokens`
    /// is the post-compaction context estimate.
    AutoCompactDone { note: String, est_tokens: u64 },
    /// A provider round completed: `round` is 1-based, `usage` is the
    /// running per-task totals. Lets UIs show progress during long runs.
    RoundDone {
        round: u32,
        usage: crate::llm::Usage,
        est_tokens: u64,
    },
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: ToolRegistry,
    pub history: context::History,
    pub ctx: ToolCtx,
    max_rounds: u32,
    max_context_tokens: u64,
    /// Max tool calls dispatched concurrently within one round; results are
    /// re-ordered back to call order so wire ids stay consistent.
    max_parallel_tools: usize,
    /// Token usage of the CURRENT task (reset at run_task start).
    usage: Usage,
    /// Set by any UI to interrupt a running task; checked between rounds and
    /// while streaming (the in-flight provider response is dropped, the
    /// agent and its history survive).
    cancel: Arc<AtomicBool>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: ToolRegistry,
        ctx: ToolCtx,
        system: Option<String>,
        max_rounds: u32,
        max_context_tokens: u64,
    ) -> Self {
        Self::with_parallel(
            provider,
            registry,
            ctx,
            system,
            max_rounds,
            max_context_tokens,
            4,
        )
    }

    /// Variants that want explicit control over per-round parallelism.
    #[allow(clippy::too_many_arguments)]
    pub fn with_parallel(
        provider: Arc<dyn Provider>,
        registry: ToolRegistry,
        ctx: ToolCtx,
        system: Option<String>,
        max_rounds: u32,
        max_context_tokens: u64,
        max_parallel_tools: usize,
    ) -> Self {
        Self {
            provider,
            registry,
            history: context::History::new(system),
            ctx,
            max_rounds: max_rounds.max(1),
            max_context_tokens,
            max_parallel_tools: max_parallel_tools.max(1),
            usage: Usage::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Handle for interrupting a running task (Esc in the TUI).
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Clear the flag; call before starting a new task.
    pub fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn max_rounds(&self) -> u32 {
        self.max_rounds
    }

    /// Token usage of the current ([`Agent::run_task`]) task.
    pub fn usage(&self) -> Usage {
        self.usage.clone()
    }

    /// All-inclusive constructor for the CLI (and embedders happy with the
    /// same wire-up): resolve provider from config, fetch key from env, build
    /// the standard tool registry + ToolCtx, load MCP servers (failures skip
    /// with a warning), prepend the default system prompt.
    /// `provider` follows resolve()'s precedence (flag > env > config > first).
    pub async fn default_cli(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        yolo: bool,
    ) -> anyhow::Result<Self> {
        Self::default_cli_with_approver(cfg, provider, model_override, approver(yolo)).await
    }

    /// Like [`Agent::default_cli`], but the caller supplies the approver —
    /// the TUI injects a channel-based one so prompts never touch stdin.
    pub async fn default_cli_with_approver(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        given_approver: Arc<dyn crate::tools::Approver>,
    ) -> anyhow::Result<Self> {
        let resolved = cfg.resolve(provider, model_override)?;
        let mut settings = resolved.settings;
        settings.api_key = cfg.api_key_for(&resolved.nick)?;
        let inner_provider = crate::llm::build_provider(&settings)?;
        let wrapped: Arc<dyn Provider> = Arc::new(crate::llm::RetryProvider::new(
            inner_provider,
            cfg.agent.max_retries.unwrap_or(3),
        ));
        let mut registry = crate::tools::ToolRegistry::builtin();
        for tool in crate::mcp::load_mcp_tools(
            &cfg.mcp,
            std::time::Duration::from_secs(crate::mcp::MCP_CALL_TIMEOUT_SECS),
        )
        .await
        {
            registry = registry.register(tool);
        }

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = crate::tools::tool_ctx(&cfg.tools, &cwd, given_approver);
        Ok(Self::with_parallel(
            wrapped,
            registry,
            ctx,
            Some(system_prompt(&cwd)),
            cfg.agent.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS),
            cfg.agent
                .max_context_tokens
                .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS),
            cfg.agent.max_parallel_tools.unwrap_or(DEFAULT_MAX_PARALLEL),
        ))
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Drop the conversation but keep the system prompt.
    pub fn reset(&mut self) {
        self.history.reset_msgs();
    }

    /// Minimal provider reachability/model-validity probe: one non-streaming
    /// round with a trivial prompt and no tools. Used by `/model` (and any
    /// embedder) to verify a model name BEFORE switching, since an invalid
    /// name otherwise surfaces only on the next real call. Never touches
    /// history or usage.
    pub async fn probe(&mut self) -> Result<(), AgentError> {
        let req = LlmRequest {
            messages: vec![Message::user("ping")],
            tools: vec![],
        };
        self.provider.complete(req).await?;
        Ok(())
    }

    /// Manual compaction (`/compact`): pull overshooting turns OUT of the
    /// history, ask the provider to compress them into a short summary, and
    /// splice the summary into the system prompt. Plain trimming remains the
    /// fallback when the summarizer call fails — compaction never loses the
    /// latest turn either way.
    pub async fn compact(&mut self) -> Result<String, AgentError> {
        let Some(dropped) = self.history.take_beyond_text(self.max_context_tokens) else {
            return Ok("nothing to compact — context is already under budget".into());
        };
        match self.summarize_dropped(&dropped).await {
            Ok(summary) if !summary.is_empty() => {
                self.history.insert_summary(summary);
                Ok("compacted: dropped turns were replaced by a summary".into())
            }
            _ => Ok("compacted: dropped turns without summary (summarizer unavailable)".into()),
        }
    }

    /// Auto-compact trigger point: at this share of the token budget the next
    /// round preemptively compacts (summary replaces dropped turns) instead of
    /// letting plain trimming silently lose them mid-task.
    const COMPACT_THRESHOLD: f64 = 0.8;

    /// Proactive compaction, called between rounds inside `run_task`. Returns
    /// `Some(note)` when it acted. Compacts whatever overshoots the budget so
    /// the summary has headroom before the next provider call. On summarizer
    /// failure plain `trim_to_tokens` remains the safety net (same as the
    /// manual path).
    pub async fn maybe_auto_compact(
        &mut self,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Option<String> {
        let budget = self.max_context_tokens;
        if (self.history.estimate_tokens() as f64) < budget as f64 * Self::COMPACT_THRESHOLD {
            return None;
        }
        let dropped = self.history.take_beyond_text(budget)?;
        on_event(AgentEvent::AutoCompactStarted);
        let note: String = match self.summarize_dropped(&dropped).await {
            Ok(summary) if !summary.is_empty() => {
                self.history.insert_summary(summary);
                "auto-compacted: older turns replaced by a summary".into()
            }
            _ => {
                // summarizer unavailable → fall back to silent trimming so
                // the next round still fits
                self.history.trim_to_tokens(budget);
                "auto-compacted: older turns dropped (summarizer unavailable)".into()
            }
        };
        on_event(AgentEvent::AutoCompactDone {
            note: note.clone(),
            est_tokens: self.history.estimate_tokens(),
        });
        Some(note)
    }

    const SUMMARY_PROMPT: &'static str = "Compress the following agent conversation transcript for a compacted context window. Keep, as terse bullets: the user's goal, decisions taken, important file paths and messages, open questions, and pending steps. Max 30 lines. No prose introductions.";

    async fn summarize_dropped(&self, transcript: &str) -> Result<String, AgentError> {
        let req = LlmRequest {
            messages: vec![
                Message::system(Self::SUMMARY_PROMPT),
                Message::user(transcript.to_string()),
            ],
            tools: vec![],
        };
        let resp = self.provider.complete(req).await?;
        Ok(resp.text_or_empty().to_string())
    }

    /// Drive one user task to its final assistant answer.
    pub async fn run_task(
        &mut self,
        task: &str,
        mut on_event: impl FnMut(AgentEvent),
    ) -> Result<String, AgentError> {
        self.history.push(Message::user(task.to_string()));
        self.usage = Usage::default();
        self.clear_cancel();

        for round in 0..self.max_rounds {
            if self.cancelled() {
                return Err(AgentError::Cancelled);
            }
            let req = LlmRequest {
                messages: self.history.messages(),
                tools: self.registry.specs(),
            };
            let resp = self.stream_round(&req, &mut on_event).await?;
            self.usage.add(&resp.usage);
            on_event(AgentEvent::RoundDone {
                round: round + 1,
                usage: self.usage.clone(),
                est_tokens: self.history.estimate_tokens(),
            });

            if !resp.has_tool_calls() {
                let text = resp.text_or_empty().to_string();
                self.history.push(Message::assistant(text.clone()));
                return Ok(text);
            }

            // ---- tool round ------------------------------------------
            // 1) Serial pre-flight: only tools that gate on approval get
            //    asked (one at a time, so prompts never interleave).
            // 2) Everything remaining is dispatched concurrently behind a
            //    semaphore; completions stream ToolEnd as they land.
            // 3) History is appended in CALL order — ids must not shuffle.
            let calls = resp.tool_calls.clone();
            self.history.push(Message::assistant_with_calls(
                resp.content.clone(),
                calls.clone(),
            ));

            let mut verdicts: Vec<Option<ToolOutcome>> = vec![None; calls.len()];
            let mut pending: Vec<(usize, ToolCall)> = Vec::new();
            for (i, call) in calls.iter().enumerate() {
                if let Some(summary) = self.registry.approval_summary(call, &self.ctx) {
                    on_event(AgentEvent::ToolStart {
                        name: call.name.clone(),
                    });
                    match self.ctx.approver.decide(&call.name, &summary).await {
                        crate::tools::Decision::Deny => {
                            let denied = ToolOutcome::err(crate::tools::shell::COMMAND_DENIED);
                            on_event(AgentEvent::ToolEnd {
                                name: call.name.clone(),
                                is_error: true,
                                preview: denied.first_line_preview(160),
                            });
                            verdicts[i] = Some(denied);
                        }
                        // Allow / AllowSession (the approver already recorded
                        // session memory) both run the call now.
                        _ => pending.push((i, call.clone())),
                    }
                } else {
                    on_event(AgentEvent::ToolStart {
                        name: call.name.clone(),
                    });
                    pending.push((i, call.clone()));
                }
            }

            let registry = self.registry.clone();
            let ctx = self.ctx.clone();
            let sem = Arc::new(tokio::sync::Semaphore::new(self.max_parallel_tools));
            let mut set = tokio::task::JoinSet::new();
            for (i, call) in pending {
                let permit = sem.clone();
                let registry = registry.clone();
                let ctx = ctx.clone();
                set.spawn(async move {
                    let _permit = permit.acquire_owned().await.expect("semaphore open");
                    let outcome = registry.execute(&call, &ctx).await;
                    (i, outcome)
                });
            }
            while let Some(done) = set.join_next().await {
                let (i, outcome) = done.expect("task join");
                on_event(AgentEvent::ToolEnd {
                    name: calls[i].name.clone(),
                    is_error: outcome.is_error,
                    preview: outcome.first_line_preview(160),
                });
                verdicts[i] = Some(outcome);
            }

            for (i, call) in calls.iter().enumerate() {
                let outcome = std::mem::take(&mut verdicts[i]).expect("every call gets an outcome");
                self.history.push(Message::tool_result(
                    &call.id,
                    outcome.text,
                    outcome.is_error,
                ));
            }
            self.history.trim_to_tokens(self.max_context_tokens);

            // Proactive compaction AFTER the tool round (before the next
            // provider call): once history sits at ≥80% of the budget, older
            // turns become a summary instead of the next trim silently
            // dropping them. Placed here — not at loop top — so the ordering
            // with the scripted-provider test responses stays intuitive.
            self.maybe_auto_compact(&mut on_event).await;
        }
        Err(AgentError::RoundsExceeded(self.max_rounds))
    }

    /// One provider round, relaying streaming text deltas as they arrive.
    async fn stream_round(
        &self,
        req: &LlmRequest,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> Result<LlmResponse, AgentError> {
        let (tx, mut rx) = mpsc::channel(64);
        let fut = self.provider.complete_stream(req.clone(), tx);
        tokio::pin!(fut);

        // Relay deltas as they land; the loop resolves when the provider
        // future completes (it owns the sender for its whole lifetime).
        let resp = loop {
            let mut closed = false;
            tokio::select! {
                ev = rx.recv(), if !closed => match ev {
                    Some(StreamEvent::TextDelta(t)) => on_event(AgentEvent::TextDelta { text: t }),
                    _ => closed = true,
                },
                r = &mut fut => break r?,
                // cancellation: poll the flag twice a second while streaming;
                // dropping `fut` cancels the in-flight HTTP response
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    if self.cancelled() {
                        return Err(AgentError::Cancelled);
                    }
                }
            }
            if closed {
                // Sender vanished with the future still pending: stop waiting
                // on the channel and let the future drive us to a verdict.
                tokio::select! {
                    r = &mut fut => break r?,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        // retry channel reading — defensive, should not happen
                    }
                }
            }
        };
        // Drain late events so nothing arrives after the final response.
        while let Some(StreamEvent::TextDelta(t)) = rx.recv().await {
            on_event(AgentEvent::TextDelta { text: t });
        }
        Ok(resp)
    }

    #[cfg(test)]
    fn history_msgs_len(&self) -> usize {
        self.history.msgs_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::mock::MockProvider;
    use crate::llm::types::{ToolCall, Usage};
    use crate::tools::{AutoApprover, ToolCtx};
    use serde_json::json;
    use std::sync::OnceLock;
    use std::time::Duration;

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(AutoApprover),
        }
    }

    /// A scratch working dir for the happy-path integration test.
    static SCRATCH: OnceLock<std::path::PathBuf> = OnceLock::new();
    fn scratch() -> &'static std::path::Path {
        let path = SCRATCH.get_or_init(|| {
            let id = std::process::id() as u64
                + std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
            let dir = std::env::temp_dir().join(format!("kazi-agent-scratch-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        });
        path
    }

    fn agent_with(script: Vec<LlmResponse>) -> Agent {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::new(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("systems".into()),
            10,
            60_000,
        );
        agent.ctx.cwd = scratch().to_path_buf();
        agent
    }

    fn agent_with_ctx(script: Vec<LlmResponse>, ctx: ToolCtx) -> Agent {
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            ctx,
            Some("systems".into()),
            10,
            60_000,
            4,
        )
    }

    #[tokio::test]
    async fn plain_answer_round() {
        let mut a = agent_with(vec![LlmResponse {
            content: Some("hi there".into()),
            ..Default::default()
        }]);
        let out = a.run_task("hello", |_| {}).await.unwrap();
        assert_eq!(out, "hi there");
        assert_eq!(a.history_msgs_len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn tool_call_round_then_final() {
        std::fs::write(scratch().join("note.txt"), b"first line\nsecond line").unwrap();
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "note.txt"}),
                }],
                stop_reason: Some("tool_use".into()),
                usage: Usage::default(),
                content: None,
            },
            LlmResponse {
                content: Some("the note says: first line".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<AgentEvent>::new()));
        let ev_events = events.clone();
        let out = a
            .run_task("read note.txt", move |e| ev_events.lock().unwrap().push(e))
            .await
            .unwrap();
        assert!(out.contains("first line"));
        assert_eq!(a.history_msgs_len(), 4); // user, assistant(calls), tool, assistant
        let evs = events.lock().unwrap();
        // RoundDone (new progress event) may interleave; match presence, not position
        assert!(evs
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { ref name } if name == "read_file")));
        assert!(evs.iter().any(
            |e| matches!(e, AgentEvent::ToolEnd { is_error: false, ref name, .. } if name == "read_file")
        ));
        assert!(evs
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("note says"))));
    }

    #[tokio::test]
    async fn unknown_tool_reports_error_and_loop_recoverable() {
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "does_not_exist".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("ok, no such tool".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        let out = a.run_task("try a fake tool", |_| {}).await.unwrap();
        assert_eq!(out, "ok, no such tool");
        assert_eq!(a.history_msgs_len(), 4);
    }

    #[tokio::test]
    async fn failing_tool_feeds_error_text_back() {
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "c9".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "missing.txt"}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("cannot read it".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        let out = a.run_task("read missing", |_| {}).await.unwrap();
        assert_eq!(out, "cannot read it");
        // the tool result text must carry the failure
        let msgs = a.history.messages();
        assert!(msgs
            .iter()
            .any(|m| m.role == crate::llm::Role::Tool && m.is_error));
    }

    #[tokio::test]
    async fn compact_summarizes_dropped_turns() {
        // two tasks → two turns; the first is dropped and summarized
        let script = vec![
            LlmResponse {
                content: Some("task one finished".into()),
                ..Default::default()
            },
            LlmResponse {
                content: Some("task two finished".into()),
                ..Default::default()
            },
            LlmResponse {
                content: Some("BULLET-1 goal; BULLET-2 pending".into()), // summarizer
                ..Default::default()
            },
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("sys".into()),
            10,
            120, // small budget: turn 1 must overshoot
            4,
        );
        agent.ctx.cwd = scratch().to_path_buf();
        let pad = " ".repeat(700); // push token estimate over the budget
        agent
            .run_task(&format!("turn one {pad}"), |_| {})
            .await
            .unwrap();
        agent.run_task("turn two", |_| {}).await.unwrap();
        assert!(agent.history.msgs_len() >= 3, "two turns stored");

        let note = agent.compact().await.unwrap();
        assert!(note.contains("summary"), "got: {note}");
        let msgs = agent.history.messages();
        assert!(
            msgs.iter()
                .any(|m| m.role == crate::llm::Role::System && m.text().contains("BULLET-1")),
            "summary must ride the system prompt"
        );
        assert!(
            !msgs
                .iter()
                .any(|m| m.role == crate::llm::Role::User && m.text().contains("turn one")),
            "dropped turn's user message must be gone"
        );
    }

    #[tokio::test]
    async fn compact_noop_when_under_budget() {
        let mut a = agent_with(vec![LlmResponse {
            content: Some("small talk".into()),
            ..Default::default()
        }]);
        a.run_task("tiny", |_| {}).await.unwrap();
        let note = a.compact().await.unwrap();
        assert!(note.starts_with("nothing to compact"), "got: {note}");
    }

    #[tokio::test]
    async fn auto_compact_fires_at_threshold_mid_task() {
        // Direct trigger test: pad history past 80% of a tight budget, then a
        // provider call (the summarizer) must compress older turns in place.
        let pad = " ".repeat(600); // ≈ 180 tokens
        let script = vec![
            LlmResponse {
                content: Some("round one checking".into()),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "list_dir".into(),
                    arguments: json!({"path": "."}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("round two checking".into()),
                tool_calls: vec![ToolCall {
                    id: "t2".into(),
                    name: "list_dir".into(),
                    arguments: json!({"path": "."}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("SUMMARY-BULLETS".into()), // summarizer after round 2
                ..Default::default()
            },
            LlmResponse {
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("sys".into()),
            10,
            400, // 0.8 × 400 = 320 triggers the check
            4,
        );
        agent.ctx.cwd = scratch().to_path_buf();
        let answer = agent
            .run_task(&format!("big task {pad}"), |_| {})
            .await
            .unwrap();
        // Round 3's final answer comes after the between-rounds compaction
        assert_eq!(answer, "done");
        // round-1/2 tool rounds are already summarized; verify splicing here
        assert!(
            agent
                .history
                .messages()
                .iter()
                .any(|m| m.role == crate::llm::Role::System
                    && m.text().contains("SUMMARY-BULLETS")),
            "summary must have been spliced by run_task's auto-compact"
        );
    }

    #[tokio::test]
    async fn auto_compact_runs_between_rounds_of_run_task() {
        // Wires the loop-level trigger through run_task: two tool-bearing
        // rounds push the history across the threshold, the between-rounds
        // check must consume the summarizer response and splice the summary,
        // and the task then finishes with round 3's answer.
        let big = " ".repeat(300); // a few hundred tokens of padding per turn
        let script = vec![
            LlmResponse {
                content: Some(format!("checking {big}")),
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "list_dir".into(),
                    arguments: json!({"path": "."}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some(format!("still checking {big}")),
                tool_calls: vec![ToolCall {
                    id: "t2".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "Cargo.toml"}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("SUMMARY-BULLETS".into()), // summarizer between rounds
                ..Default::default()
            },
            LlmResponse {
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("sys".into()),
            5,
            600, // trigger at 480; two padded tool rounds blow past it
            4,
        );
        agent.ctx.cwd = scratch().to_path_buf();

        let events: std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>> = Default::default();
        let sink = events.clone();
        let answer = agent
            .run_task(&format!("huge {big}"), move |ev| {
                sink.lock().unwrap().push(ev);
            })
            .await
            .unwrap();
        assert_eq!(answer, "done", "run must finish after compaction");
        let evs = events.lock().unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(e, AgentEvent::AutoCompactStarted)),
            "run_task must trigger auto-compact between rounds, events: {evs:?}"
        );
        assert!(evs
            .iter()
            .any(|e| matches!(e, AgentEvent::AutoCompactDone { .. })));
    }

    #[tokio::test]
    async fn auto_compact_falls_back_to_trim_when_summarizer_fails() {
        // Script exhausted after round 1's tool call → summarizer call errors
        // → fallback path must silently trim, and the task must still finish.
        let pad = " ".repeat(600);
        let script = vec![LlmResponse {
            content: Some("checking".into()),
            tool_calls: vec![ToolCall {
                id: "t1".into(),
                name: "list_dir".into(),
                arguments: json!({"path": "."}),
            }],
            ..Default::default()
        }];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("sys".into()),
            3,
            400,
            4,
        );
        agent.ctx.cwd = scratch().to_path_buf();

        let result = agent.run_task(&format!("big task {pad}"), |_| {}).await;
        // MockProvider errors when the script runs dry → the run ends with a
        // Provider error, not a context blowup — trim kept the request legal.
        assert!(result.is_err(), "script runs dry: {result:?}");
        let est = agent.history.estimate_tokens();
        assert!(
            est <= 400,
            "fallback trim must bring context under the budget: {est}"
        );
    }

    #[tokio::test]
    async fn auto_compact_silent_when_small() {
        let script = vec![LlmResponse {
            content: Some("tiny answer".into()),
            ..Default::default()
        }];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            test_ctx(),
            Some("sys".into()),
            5,
            60_000,
            4,
        );
        agent.ctx.cwd = scratch().to_path_buf();
        let events: std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>> = Default::default();
        let sink = events.clone();
        agent
            .run_task("tiny", move |ev| {
                sink.lock().unwrap().push(ev);
            })
            .await
            .unwrap();
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::AutoCompactStarted)),
            "small task must never trigger auto-compact"
        );
    }

    #[tokio::test]
    async fn usage_accumulates_per_round_and_resets() {
        let script = vec![
            LlmResponse {
                usage: Usage {
                    input_tokens: Some(11),
                    output_tokens: Some(22),
                },
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "list_dir".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            LlmResponse {
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                },
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script.clone());
        a.run_task("first", |_| {}).await.unwrap();
        let u = a.usage();
        assert_eq!(u.input_tokens, Some(21));
        assert_eq!(u.output_tokens, Some(27));
        // the next task starts with a fresh counter
        let last = script.last().cloned().unwrap();
        let mut a = agent_with(vec![last]);
        a.run_task("second", |_| {}).await.unwrap();
        assert_eq!(a.usage().input_tokens, Some(10));
    }

    #[tokio::test]
    async fn parallel_tool_calls_execute_both_in_call_order() {
        std::fs::write(scratch().join("note.txt"), b"first line").unwrap();
        let script = vec![
            LlmResponse {
                tool_calls: vec![
                    ToolCall {
                        id: "c-a".into(),
                        name: "list_dir".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c-b".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "note.txt"}),
                    },
                ],
                ..Default::default()
            },
            LlmResponse {
                content: Some("both done".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        let out = a.run_task("run two tools", |_| {}).await.unwrap();
        assert_eq!(out, "both done");
        // system, user, assistant(2 calls), tool(a), tool(b), assistant
        let msgs = a.history.messages();
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("c-a"));
        assert_eq!(msgs[4].tool_call_id.as_deref(), Some("c-b"));
        assert!(!msgs[4].is_error);
    }

    #[tokio::test]
    async fn denied_command_isolated_other_tools_still_run() {
        use crate::tools::DenyApprover;
        std::fs::write(scratch().join("note.txt"), b"first line").unwrap();
        let ctx = ToolCtx {
            cwd: scratch().to_path_buf(),
            sandbox_root: None,
            timeout: Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(DenyApprover),
        };
        let script = vec![
            LlmResponse {
                tool_calls: vec![
                    ToolCall {
                        id: "c-a".into(),
                        name: "run_command".into(),
                        arguments: json!({"command": "echo hi"}),
                    },
                    ToolCall {
                        id: "c-b".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "note.txt"}),
                    },
                ],
                ..Default::default()
            },
            LlmResponse {
                content: Some("recovered".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with_ctx(script, ctx);
        let out = a.run_task("mixed approvals", |_| {}).await.unwrap();
        assert_eq!(out, "recovered");
        let msgs = a.history.messages();
        let denied = &msgs[3]; // system, user, assistant(calls), tool(denied)
        let ran = &msgs[4];
        assert_eq!(denied.tool_call_id.as_deref(), Some("c-a"));
        assert!(denied.is_error, "denial must be an error result");
        assert_eq!(denied.text(), crate::tools::shell::COMMAND_DENIED);
        assert_eq!(ran.tool_call_id.as_deref(), Some("c-b"));
        assert!(!ran.is_error, "read_file must still have run");
        assert!(ran.text().contains("first line"));
    }

    #[tokio::test]
    async fn cancel_token_stops_the_task_between_rounds() {
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "a".into(),
                    name: "list_dir".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("should never be reached".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        let flag = a.cancel_token();
        let out = a
            .run_task("work then cancel", move |ev| {
                if matches!(ev, AgentEvent::ToolStart { .. }) {
                    flag.store(true, Ordering::Relaxed);
                }
            })
            .await;
        assert!(matches!(out.unwrap_err(), AgentError::Cancelled));
        // the conversation is preserved (user + assistant-with-calls + tool)
        assert!(a
            .history
            .messages()
            .iter()
            .any(|m| m.role == crate::llm::Role::User));
    }

    #[tokio::test]
    async fn rounds_exceeded_is_an_agent_error() {
        // Tool call every round → budget exhaustion.
        let calls = (0..12)
            .map(|i| LlmResponse {
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    name: "list_dir".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            })
            .collect();
        let mut a = agent_with(calls);
        let err = a.run_task("spinning", |_| {}).await.unwrap_err();
        assert!(matches!(err, AgentError::RoundsExceeded(_)));
    }
}
