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

/// Expand `@path` references in a task into attached file contents (capped at
/// 64 KiB per file, binaries skipped, missing files reported inline). The
/// model sees the augmented text; the transcript still shows what the user
/// typed.
pub fn expand_file_refs(task: &str, cwd: &std::path::Path) -> String {
    const CAP: usize = 64 * 1024;
    let mut attachments: Vec<String> = Vec::new();
    for raw in task.split_whitespace() {
        let Some(path) = raw.strip_prefix('@') else {
            continue;
        };
        let path = path.trim_end_matches(|c: char| {
            matches!(c, ',' | '.' | ';' | ':' | ')' | ']' | '"' | '\'')
        });
        if path.is_empty() {
            continue;
        }
        let full = cwd.join(path);
        match std::fs::read(&full) {
            Ok(bytes) if bytes.iter().take(8192).any(|b| *b == 0) => {
                attachments.push(format!("[attached @{path}: binary, skipped]"));
            }
            Ok(bytes) => {
                let truncated = bytes.len() > CAP;
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(CAP)]);
                attachments.push(format!(
                    "[attached @{path}: {} bytes{}]\n{text}",
                    bytes.len(),
                    if truncated { ", truncated" } else { "" }
                ));
            }
            Err(_) => attachments.push(format!("[cannot attach @{path}: not readable]")),
        }
    }
    if attachments.is_empty() {
        task.to_string()
    } else {
        format!("{task}\n\n{}", attachments.join("\n\n"))
    }
}

/// Run the post-edit feedback check (`agent.feedback_command`) and shape its
/// output for the model: `(ok, one-line preview, full message)`.
async fn run_feedback(cmd: &str, ctx: &crate::tools::ToolCtx) -> (bool, String, String) {
    const TAIL_CAP: usize = 4_000;
    let mut c = crate::tools::shell::build_command(cmd, &ctx.cwd);
    c.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let output = match c.spawn() {
        Ok(child) => match tokio::time::timeout(ctx.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return (
                    false,
                    format!("failed to run: {e}"),
                    format!("[feedback] `{cmd}` failed to run: {e}"),
                );
            }
            Err(_) => {
                return (
                    false,
                    "timed out".to_string(),
                    format!(
                        "[feedback] `{cmd}` timed out after {}s",
                        ctx.timeout.as_secs()
                    ),
                );
            }
        },
        Err(e) => {
            return (
                false,
                format!("cannot start: {e}"),
                format!("[feedback] `{cmd}` cannot start: {e}"),
            );
        }
    };
    let code = output.status.code().unwrap_or(-1);
    let ok = code == 0;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    // keep the TAIL — compilers put the actual errors at the end
    let total = text.chars().count();
    let tail: String = if total > TAIL_CAP {
        text.chars().skip(total - TAIL_CAP).collect()
    } else {
        text
    };
    let preview: String = tail
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect();
    let body = format!(
        "[feedback] `{cmd}` exited {code}{}{}\n{tail}",
        if ok {
            ""
        } else {
            " — fix these before finishing"
        },
        if total > TAIL_CAP {
            " (output tail)"
        } else {
            ""
        }
    );
    (ok, preview, body)
}

/// Sub-agent factory backing the `delegate` tool: same provider, a fresh
/// (short) history, and an isolated tool context.
pub struct SubAgentRunnerImpl {
    provider: Arc<dyn Provider>,
    base_ctx: crate::tools::ToolCtx,
    system: String,
    max_rounds: u32,
    max_context_tokens: u64,
}

impl SubAgentRunnerImpl {
    pub fn new(
        provider: Arc<dyn Provider>,
        base_ctx: crate::tools::ToolCtx,
        system: String,
        max_rounds: u32,
        max_context_tokens: u64,
    ) -> Self {
        Self {
            provider,
            base_ctx,
            system,
            max_rounds,
            max_context_tokens,
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::subagent::SubAgentRunner for SubAgentRunnerImpl {
    async fn run_sub(&self, prompt: &str, read_only: bool) -> Result<String, String> {
        const REPORT_CAP: usize = 4_000;
        let mut ctx = self.base_ctx.clone();
        ctx.subagent = None; // delegation cannot nest
        ctx.checkpointing = None; // sub-agent edits are not a /undo epoch
        ctx.todos = None; // its plan is its own
        if read_only {
            ctx.permission.set(crate::tools::PermissionMode::ReadOnly);
        }
        let mut sub = Agent::with_parallel(
            self.provider.clone(),
            crate::tools::ToolRegistry::builtin(),
            ctx,
            Some(self.system.clone()),
            self.max_rounds.clamp(1, 12),
            self.max_context_tokens,
            4,
        );
        match sub.run_task(prompt, |_| {}).await {
            Ok(text) => {
                let total = text.chars().count();
                Ok(if total > REPORT_CAP {
                    text.chars().take(REPORT_CAP).collect::<String>()
                        + "\n[…sub-agent report truncated]"
                } else {
                    text
                })
            }
            Err(e) => Err(format!("sub-agent failed: {e}")),
        }
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
    /// The model's plan checklist changed: `(content, status)` pairs, in
    /// order. Emitted after a tool round when `todo_write` ran.
    TodosUpdated { items: Vec<(String, String)> },
    /// Tool call finished: error flag + one-line preview of its output.
    ToolEnd {
        name: String,
        is_error: bool,
        preview: String,
    },
    /// Auto-compact kicking in between rounds (proactive, at 80% budget).
    AutoCompactStarted,
    /// Post-edit feedback command finished (`agent.feedback_command`).
    Feedback {
        command: String,
        ok: bool,
        preview: String,
    },
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
    /// Optional post-edit check (`agent.feedback_command`): run after a round
    /// that changed files, its output is fed back to the model.
    feedback: Option<String>,
    /// Provider nickname this agent resolved to (`[providers.<nick>]`).
    nick: String,
    /// Set when the provider has no usable credential: an actionable line the
    /// UI shows at startup and appends to auth-ish provider errors.
    auth_hint: Option<String>,
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
            feedback: None,
            nick: String::new(),
            auth_hint: None,
        }
    }

    /// Install the post-edit feedback command (builder-style).
    pub fn with_feedback(mut self, command: Option<String>) -> Self {
        self.feedback = command.filter(|c| !c.trim().is_empty());
        self
    }

    /// Handle for interrupting a running task (Esc in the TUI).
    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Provider nickname this agent resolved to.
    pub fn nick(&self) -> &str {
        &self.nick
    }

    /// Actionable key hint when the provider has no usable credential
    /// (env var and local store both empty).
    pub fn auth_hint(&self) -> Option<&str> {
        self.auth_hint.as_deref()
    }

    /// Error text for the UI: provider failures that look like a missing or
    /// rejected key gain the exact command that fixes them.
    pub fn error_text(&self, e: &AgentError) -> String {
        let base = e.to_string();
        let authish = matches!(e, AgentError::Provider(_))
            && ["authentication", "API key", "401", "authorization"]
                .iter()
                .any(|needle| base.contains(needle));
        match (&self.auth_hint, authish) {
            (Some(hint), true) => format!("{base}\n→ {hint}"),
            _ => base,
        }
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

    /// Context budget in estimated tokens (`/context` display).
    pub fn max_context_tokens(&self) -> u64 {
        self.max_context_tokens
    }

    /// Share of the budget at which auto-compaction fires (percent) — reported
    /// by `/context` so the number is not a mystery.
    pub fn compact_threshold_percent() -> u32 {
        (Self::COMPACT_THRESHOLD * 100.0) as u32
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
        let mode = Self::initial_mode(cfg, yolo);
        Self::default_cli_with_approver(cfg, provider, model_override, approver(yolo), mode).await
    }

    /// Startup permission mode: `--yolo` wins, then `agent.permission_mode`,
    /// then the `Edit` default.
    pub fn initial_mode(cfg: &crate::config::Config, yolo: bool) -> crate::tools::PermissionMode {
        if yolo {
            return crate::tools::PermissionMode::Yolo;
        }
        cfg.agent
            .permission_mode
            .as_deref()
            .and_then(crate::tools::PermissionMode::parse)
            .unwrap_or(crate::tools::PermissionMode::Edit)
    }

    /// Mode-aware constructor for the CLI: `Yolo` implies the permissive
    /// approver, everything else keeps the interactive gate.
    pub async fn default_cli_mode(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        mode: crate::tools::PermissionMode,
    ) -> anyhow::Result<Self> {
        let a = approver(mode == crate::tools::PermissionMode::Yolo);
        Self::default_cli_with_approver(cfg, provider, model_override, a, mode).await
    }

    /// Resolve provider settings (key included) + the actionable hint when no
    /// credential is available. Shared by every constructor.
    fn resolved_settings(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
    ) -> anyhow::Result<(crate::llm::ProviderSettings, String, Option<String>)> {
        let resolved = cfg.resolve(provider, model_override)?;
        let mut settings = resolved.settings;
        let hint = match cfg.api_key_for(&resolved.nick) {
            Ok(key) => {
                settings.api_key = key;
                None
            }
            Err(e) => {
                let env_name = cfg
                    .key_sources(&resolved.nick)
                    .map(|(env, _, _)| env)
                    .unwrap_or_default();
                eprintln!("warning: {e}");
                eprintln!("         fix it with: kazi auth set {}", resolved.nick);
                Some(format!(
                    "no usable key for `{nick}`: run `kazi auth set {nick}`{env} — then reconnect this session with /provider {nick}",
                    nick = resolved.nick,
                    env = if env_name.is_empty() {
                        String::new()
                    } else {
                        format!(" (or export {env_name})")
                    },
                ))
            }
        };
        Ok((settings, resolved.nick, hint))
    }

    /// Wrap the wire provider with the retry policy from config.
    fn provider_arc(
        cfg: &crate::config::Config,
        settings: crate::llm::ProviderSettings,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let inner = crate::llm::build_provider(&settings)?;
        Ok(Arc::new(crate::llm::RetryProvider::new(
            inner,
            cfg.agent.max_retries.unwrap_or(3),
        )))
    }

    /// Assemble an agent around an already-built provider + tool registry.
    /// Shared by the constructors so a provider/model switch can reuse the live
    /// registry — MCP servers are provider-agnostic and must not be
    /// re-handshaked (that was the multi-second stall on every switch).
    fn assemble(
        wrapped: Arc<dyn Provider>,
        registry: ToolRegistry,
        mut ctx: ToolCtx,
        cfg: &crate::config::Config,
        nick: String,
        auth_hint: Option<String>,
    ) -> Self {
        let max_rounds = cfg.agent.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS);
        let max_context_tokens = cfg
            .agent
            .max_context_tokens
            .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS);
        let system = system_prompt(&ctx.cwd);
        let runner = Arc::new(SubAgentRunnerImpl::new(
            wrapped.clone(),
            ctx.clone(),
            system.clone(),
            max_rounds,
            max_context_tokens,
        ));
        ctx.subagent = Some(runner);
        let mut agent = Self::with_parallel(
            wrapped,
            registry,
            ctx,
            Some(system),
            max_rounds,
            max_context_tokens,
            cfg.agent.max_parallel_tools.unwrap_or(DEFAULT_MAX_PARALLEL),
        )
        .with_feedback(cfg.agent.feedback_command.clone());
        agent.nick = nick;
        agent.auth_hint = auth_hint;
        agent
    }

    /// Rebuild for a provider/model switch: same tool registry, same cwd, no
    /// MCP re-handshake, no config re-read of everything else.
    pub async fn rebuilt_with(
        &self,
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        given_approver: Arc<dyn crate::tools::Approver>,
        mode: crate::tools::PermissionMode,
    ) -> anyhow::Result<Self> {
        let (settings, nick, hint) = Self::resolved_settings(cfg, provider, model_override)?;
        let wrapped = Self::provider_arc(cfg, settings)?;
        let mut ctx = self.ctx.clone();
        ctx.approver = given_approver;
        ctx.permission = crate::tools::Permission::new(mode);
        Ok(Self::assemble(
            wrapped,
            self.registry.clone(),
            ctx,
            cfg,
            nick,
            hint,
        ))
    }

    /// Like [`Agent::default_cli`], but the caller supplies the approver —
    /// the TUI injects a channel-based one so prompts never touch stdin —
    /// and the startup permission mode.
    pub async fn default_cli_with_approver(
        cfg: &crate::config::Config,
        provider: Option<&str>,
        model_override: Option<&str>,
        given_approver: Arc<dyn crate::tools::Approver>,
        mode: crate::tools::PermissionMode,
    ) -> anyhow::Result<Self> {
        let (settings, nick, auth_hint) = Self::resolved_settings(cfg, provider, model_override)?;
        let wrapped = Self::provider_arc(cfg, settings)?;
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
        let ctx = crate::tools::tool_ctx(&cfg.tools, &cwd, given_approver, mode);
        Ok(Self::assemble(wrapped, registry, ctx, cfg, nick, auth_hint))
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

    /// Manual compaction (`/compact`): pull the oldest droppable messages OUT
    /// of the history — only as many as the budget requires; `force` drops
    /// everything droppable — ask the provider to compress them into a short
    /// summary, and splice the summary into the system prompt. When the
    /// summarizer fails the messages are gone anyway, so the note says so.
    pub async fn compact(&mut self, force: bool) -> Result<String, AgentError> {
        let before_tokens = self.history.estimate_tokens();
        let before_msgs = self.history.msgs_len();
        let target = if force { 0 } else { self.max_context_tokens };
        let Some(dropped) = self.history.take_beyond_text(target) else {
            return Ok(if force {
                "nothing to compact — no droppable turns yet".into()
            } else {
                "nothing to compact — context is already under budget (use `/compact force` to drop anyway)"
                    .into()
            });
        };
        let dropped_msgs = before_msgs - self.history.msgs_len();
        match self.summarize_dropped(&dropped).await {
            Ok(summary) if !summary.is_empty() => {
                self.history.insert_summary(summary);
                Ok(format!(
                    "compacted: {dropped_msgs} messages replaced by a summary ({before_tokens} → {} est. tokens)",
                    self.history.estimate_tokens()
                ))
            }
            _ => Ok(format!(
                "compacted: {dropped_msgs} messages DROPPED with no summary (summarizer unavailable) — lossy ({before_tokens} → {} est. tokens)",
                self.history.estimate_tokens()
            )),
        }
    }

    /// Auto-compact trigger point: at this share of the token budget the next
    /// round preemptively compacts (summary replaces dropped turns) instead of
    /// letting plain trimming silently lose them mid-task.
    const COMPACT_THRESHOLD: f64 = 0.8;
    /// Level auto-compaction compacts down to — headroom for the rounds that
    /// follow, instead of hugging the budget edge.
    const COMPACT_TARGET: f64 = 0.6;

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
        let dropped = self
            .history
            .take_beyond_text((budget as f64 * Self::COMPACT_TARGET) as u64)?;
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
        // every task run gets a fresh checkpoint epoch: `/undo` restores it
        if let Some(cp) = &self.ctx.checkpointing {
            cp.begin_epoch();
        }
        // plan snapshot across rounds: emit `TodosUpdated` only on change
        let mut last_todos: Option<Vec<(String, String)>> = None;

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
                        crate::tools::Decision::Deny(reason) => {
                            let denied = ToolOutcome::err(crate::tools::shell::command_denied(
                                reason.as_deref(),
                            ));
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
            // Esc must bite while a tool runs too: poll the flag and abort the
            // tool tasks (spawned commands are kill_on_drop, so a hung
            // `run_command` dies with its task instead of holding the run).
            let mut remaining = set.len();
            while remaining > 0 {
                tokio::select! {
                    joined = set.join_next() => match joined {
                        Some(Ok((i, outcome))) => {
                            on_event(AgentEvent::ToolEnd {
                                name: calls[i].name.clone(),
                                is_error: outcome.is_error,
                                preview: outcome.first_line_preview(160),
                            });
                            verdicts[i] = Some(outcome);
                            remaining -= 1;
                        }
                        Some(Err(_)) => remaining -= 1, // panicked tool task
                        None => break,
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                        if self.cancelled() {
                            set.abort_all();
                            return Err(AgentError::Cancelled);
                        }
                    }
                }
            }

            // did this round change files? (before outcomes are taken)
            let wrote = calls.iter().enumerate().any(|(i, c)| {
                matches!(c.name.as_str(), "write_file" | "apply_patch")
                    && verdicts[i].as_ref().map(|o| !o.is_error).unwrap_or(false)
            });

            for (i, call) in calls.iter().enumerate() {
                let outcome = std::mem::take(&mut verdicts[i]).expect("every call gets an outcome");
                self.history.push(Message::tool_result(
                    &call.id,
                    outcome.text,
                    outcome.is_error,
                ));
            }
            // surface plan changes to the UI
            if let Some(list) = &self.ctx.todos {
                let snap: Vec<(String, String)> = list
                    .snapshot()
                    .into_iter()
                    .map(|t| (t.content, t.status))
                    .collect();
                if last_todos.as_ref() != Some(&snap) {
                    last_todos = Some(snap.clone());
                    on_event(AgentEvent::TodosUpdated { items: snap });
                }
            }
            // post-edit feedback: run the configured check after a round that
            // changed files, and feed its output back to the model
            if wrote {
                if let Some(cmd) = self.feedback.clone() {
                    if self.ctx.permission.get() != crate::tools::PermissionMode::ReadOnly {
                        let (ok, preview, body) = run_feedback(&cmd, &self.ctx).await;
                        on_event(AgentEvent::Feedback {
                            command: cmd,
                            ok,
                            preview,
                        });
                        self.history.push(Message::user(body));
                    }
                }
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
            checkpointing: None,
            todos: None,
            permission: crate::tools::Permission::default(),
            subagent: None,
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

    /// A per-test directory under the scratch root: tests run in parallel and
    /// used to write the same file name into one shared dir, which raced.
    fn scratch_named(name: &str) -> std::path::PathBuf {
        let dir = scratch().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

    #[test]
    fn expands_file_refs_and_reports_failures() {
        let d = std::env::temp_dir().join(format!("kazi-refs-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.txt"), "hello file").unwrap();

        let out = expand_file_refs("summarize @a.txt please", &d);
        assert!(out.contains("[attached @a.txt"), "got: {out}");
        assert!(out.contains("hello file"));

        let miss = expand_file_refs("read @missing.rs", &d);
        assert!(miss.contains("cannot attach @missing.rs"));

        assert_eq!(expand_file_refs("no refs here", &d), "no refs here");
        let _ = std::fs::remove_dir_all(&d);
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
        let dir = scratch_named("read-round");
        std::fs::write(dir.join("note.txt"), b"first line\nsecond line").unwrap();
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
        a.ctx.cwd = dir.clone();
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

        let note = agent.compact(false).await.unwrap();
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
        // two turns under a generous budget: plain /compact must not touch them
        let script = vec![
            LlmResponse {
                content: Some("first answer".into()),
                ..Default::default()
            },
            LlmResponse {
                content: Some("second answer".into()),
                ..Default::default()
            },
            LlmResponse {
                content: Some("SUMMARY".into()), // summarizer (only `force` gets here)
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        a.run_task("tiny one", |_| {}).await.unwrap();
        a.run_task("tiny two", |_| {}).await.unwrap();
        let msgs_before = a.history.msgs_len();

        let note = a.compact(false).await.unwrap();
        assert!(note.starts_with("nothing to compact"), "got: {note}");
        assert_eq!(
            a.history.msgs_len(),
            msgs_before,
            "under budget nothing may be dropped"
        );

        // ... but `force` drops the droppable part on purpose
        let forced = a.compact(true).await.unwrap();
        assert!(
            !forced.starts_with("nothing to compact"),
            "force must act under budget: {forced}"
        );
        assert!(
            a.history.msgs_len() < msgs_before,
            "force dropped the older turn"
        );
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
    async fn delegate_returns_subagent_summary() {
        // the sub-agent runs on its own provider script
        let sub_provider: Arc<dyn Provider> =
            Arc::new(crate::llm::mock::MockProvider::new(vec![LlmResponse {
                content: Some("SUB_AGENT_FOUND: 42".into()),
                ..Default::default()
            }]));
        let mut ctx = test_ctx();
        let base = ctx.clone();
        ctx.subagent = Some(Arc::new(SubAgentRunnerImpl::new(
            sub_provider,
            base,
            "sub system".into(),
            3,
            1_000,
        )));

        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "d1".into(),
                    name: "delegate".into(),
                    arguments: json!({"prompt": "count the things"}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("summary received".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with_ctx(script, ctx);
        a.run_task("delegate the counting", |_| {}).await.unwrap();

        let joined = a
            .history
            .msgs
            .iter()
            .map(|m| m.text().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("SUB_AGENT_FOUND: 42"),
            "the sub-agent's report must reach the main history: {joined}"
        );
    }

    #[tokio::test]
    async fn feedback_command_runs_after_a_write_round() {
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "w1".into(),
                    name: "write_file".into(),
                    arguments: json!({"path": "gen.txt", "content": "generated"}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let mut a =
            agent_with_ctx(script, test_ctx()).with_feedback(Some("echo KAZI_FEEDBACK_OK".into()));
        // its own dir: `gen.txt` must not race with other tests
        a.ctx.cwd = scratch_named("feedback-round");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(bool, String)>::new()));
        let sink = seen.clone();
        a.run_task("write a file", move |ev| {
            if let AgentEvent::Feedback { ok, preview, .. } = ev {
                sink.lock().unwrap().push((ok, preview));
            }
        })
        .await
        .unwrap();

        let hits = seen.lock().unwrap();
        assert_eq!(hits.len(), 1, "one feedback run after the write round");
        assert!(hits[0].0, "echo exits 0");
        assert!(hits[0].1.contains("KAZI_FEEDBACK_OK"), "got: {:?}", hits[0]);
    }

    #[tokio::test]
    async fn todo_write_emits_plan_events() {
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "t1".into(),
                    name: "todo_write".into(),
                    arguments: json!({"todos": [
                        {"content": "step A", "status": "completed"},
                        {"content": "step B", "status": "in_progress"}
                    ]}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("planned".into()),
                ..Default::default()
            },
        ];
        let mut ctx = test_ctx();
        ctx.todos = Some(std::sync::Arc::new(crate::tools::todo::TodoList::default()));
        let mut a = agent_with_ctx(script, ctx);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<(String, String)>>::new()));
        let sink = seen.clone();
        a.run_task("plan it", move |ev| {
            if let AgentEvent::TodosUpdated { items } = ev {
                sink.lock().unwrap().push(items);
            }
        })
        .await
        .unwrap();

        let snapshots = seen.lock().unwrap();
        assert_eq!(snapshots.len(), 1, "exactly one plan event");
        assert_eq!(snapshots[0][0], ("step A".into(), "completed".into()));
        assert_eq!(snapshots[0][1], ("step B".into(), "in_progress".into()));
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
        let dir = scratch_named("parallel-round");
        std::fs::write(dir.join("note.txt"), b"first line").unwrap();
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
        a.ctx.cwd = dir.clone();
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
        let dir = scratch_named("denied-round");
        std::fs::write(dir.join("note.txt"), b"first line").unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            sandbox_root: None,
            timeout: Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(DenyApprover),
            checkpointing: None,
            todos: None,
            permission: crate::tools::Permission::default(),
            subagent: None,
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
    async fn cancel_bites_while_a_tool_runs() {
        struct SlowTool;
        #[async_trait::async_trait]
        impl crate::tools::Tool for SlowTool {
            fn name(&self) -> &'static str {
                "slow_tool"
            }
            fn description(&self) -> &'static str {
                "sleeps for a long time"
            }
            fn parameters(&self) -> serde_json::Value {
                json!({"type": "object", "properties": {}})
            }
            async fn run(
                &self,
                _args: &serde_json::Value,
                _ctx: &ToolCtx,
            ) -> Result<String, crate::tools::ToolError> {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("never reached".into())
            }
        }

        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "s1".into(),
                    name: "slow_tool".into(),
                    arguments: json!({}),
                }],
                ..Default::default()
            },
            LlmResponse {
                content: Some("should never be reached".into()),
                ..Default::default()
            },
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(script));
        let registry = ToolRegistry::new().register(Arc::new(SlowTool));
        let mut agent = Agent::with_parallel(
            provider,
            registry,
            test_ctx(),
            Some("sys".into()),
            4,
            1_000,
            4,
        );
        let flag = agent.cancel_token();
        let started = std::time::Instant::now();
        let out = agent
            .run_task("run the slow tool", move |ev| {
                if matches!(ev, AgentEvent::ToolStart { .. }) {
                    flag.store(true, Ordering::Relaxed);
                }
            })
            .await;
        assert!(matches!(out.unwrap_err(), AgentError::Cancelled));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancel must bite during a tool, took {:?}",
            started.elapsed()
        );
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
