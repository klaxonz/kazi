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

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::llm::{LlmError, LlmRequest, LlmResponse, Message, Provider, StreamEvent};
use crate::tools::{ToolCtx, ToolRegistry};

pub const DEFAULT_MAX_ROUNDS: u32 = 30;
/// Rough chat-task budget; History trimming estimates 4 chars ≈ 1 token.
pub const DEFAULT_MAX_CONTEXT_TOKENS: u64 = 60_000;

/// Approver selection: terminal prompt by default, blanket yes with `--yolo`.
fn approver(yolo: bool) -> Arc<dyn crate::tools::Approver> {
    if yolo {
        Arc::new(crate::tools::AutoApprover)
    } else {
        Arc::new(repl::StdinApprover)
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
}

/// What the UI wants to know while a task runs.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Assistant text fragment, in order.
    TextDelta { text: String },
    /// A tool call is starting.
    ToolStart { name: String },
    /// A tool call finished: error flag + one-line preview of its output.
    ToolEnd {
        name: String,
        is_error: bool,
        preview: String,
    },
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: ToolRegistry,
    pub history: context::History,
    pub ctx: ToolCtx,
    max_rounds: u32,
    max_context_tokens: u64,
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
        Self {
            provider,
            registry,
            history: context::History::new(system),
            ctx,
            max_rounds: max_rounds.max(1),
            max_context_tokens,
        }
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn max_rounds(&self) -> u32 {
        self.max_rounds
    }

    /// All-inclusive constructor for the CLI (and embedders happy with the
    /// same wire-up): resolve provider from config, fetch key from env, build
    /// the standard tool registry + ToolCtx, prepend the default system prompt.
    /// `provider` follows resolve()'s precedence (flag > env > config > first).
    pub fn default_cli(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        yolo: bool,
    ) -> anyhow::Result<Self> {
        let resolved = cfg.resolve(provider, model_override)?;
        let mut settings = resolved.settings;
        settings.api_key = cfg.api_key_for(&resolved.nick)?;
        let provider = crate::llm::build_provider(&settings)?;
        let registry = crate::tools::ToolRegistry::builtin();

        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = crate::tools::tool_ctx(&cfg.tools, &cwd, approver(yolo));
        Ok(Self::new(
            provider,
            registry,
            ctx,
            Some(system_prompt(&cwd)),
            cfg.agent.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS),
            cfg.agent
                .max_context_tokens
                .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS),
        ))
    }

    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Drop the conversation but keep the system prompt.
    pub fn reset(&mut self) {
        self.history.reset_msgs();
    }

    /// Manual compaction (`/compact`): trim oldest turns over the budget.
    pub fn compact(&mut self) {
        self.history.trim_to_tokens(self.max_context_tokens);
    }

    /// Drive one user task to its final assistant answer.
    pub async fn run_task(
        &mut self,
        task: &str,
        mut on_event: impl FnMut(AgentEvent),
    ) -> Result<String, AgentError> {
        self.history.push(Message::user(task.to_string()));

        for _ in 0..self.max_rounds {
            let req = LlmRequest {
                messages: self.history.messages(),
                tools: self.registry.specs(),
            };
            let resp = self.stream_round(&req, &mut on_event).await?;

            if !resp.has_tool_calls() {
                let text = resp.text_or_empty().to_string();
                self.history.push(Message::assistant(text.clone()));
                return Ok(text);
            }

            self.history.push(Message::assistant_with_calls(
                resp.content.clone(),
                resp.tool_calls.clone(),
            ));
            for call in &resp.tool_calls {
                on_event(AgentEvent::ToolStart {
                    name: call.name.clone(),
                });
                let outcome = self.registry.execute(call, &self.ctx).await;
                on_event(AgentEvent::ToolEnd {
                    name: call.name.clone(),
                    is_error: outcome.is_error,
                    preview: outcome.first_line_preview(160),
                });
                self.history.push(Message::tool_result(
                    &call.id,
                    outcome.text,
                    outcome.is_error,
                ));
            }
            self.history.trim_to_tokens(self.max_context_tokens);
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
        assert!(matches!(evs[0], AgentEvent::ToolStart { ref name } if name == "read_file"));
        assert!(
            matches!(evs[1], AgentEvent::ToolEnd { is_error: false, ref name, .. } if name == "read_file")
        );
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
