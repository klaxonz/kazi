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

pub mod console;
pub mod context;
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
/// Tool text embedded in `ToolEnd` events. The model still receives the full
/// (output-limit-capped) result; this only bounds what a UI/JSON stream keeps.
pub const MAX_EVENT_OUTPUT: usize = 32_000;
/// Don't try to diff files bigger than this (they are all `output_limit`
/// guarded anyway; this keeps the event loop from reading a huge file).
const MAX_DIFF_BYTES: u64 = 4 * 1024 * 1024;

/// Approver selection: terminal prompt by default, blanket yes with `--yolo`.
fn approver(yolo: bool) -> Arc<dyn crate::tools::Approver> {
    if yolo {
        Arc::new(crate::tools::AutoApprover)
    } else {
        Arc::new(console::StdinApprover::default())
    }
}

/// The path behind one raw `@token`, with trailing punctuation stripped.
fn ref_path(raw: &str) -> Option<&str> {
    let path = raw.strip_prefix('@')?;
    let path = path.trim_end_matches([',', '.', ';', ':', ')', ']', '"', '\'']);
    (!path.is_empty()).then_some(path)
}

/// The `@path` references a task mentions, deduped and in order. The TUI shows
/// this before the task runs so an attachment is never invisible.
pub fn file_ref_names(task: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in task.split_whitespace() {
        if let Some(path) = ref_path(raw) {
            if !out.iter().any(|p| p == path) {
                out.push(path.to_string());
            }
        }
    }
    out
}

/// Expand `@path` references in a task into attached file contents (capped at
/// 64 KiB per file, binaries skipped, missing files reported inline). The
/// model sees the augmented text; the transcript still shows what the user
/// typed.
pub fn expand_file_refs(task: &str, cwd: &std::path::Path) -> String {
    const CAP: usize = 64 * 1024;
    let mut attachments: Vec<String> = Vec::new();
    for raw in task.split_whitespace() {
        let Some(path) = ref_path(raw) else {
            continue;
        };
        if image_media_type(path).is_some() {
            // images travel as message parts, not as text (see
            // `expand_image_refs`); one line keeps the transcript honest
            attachments.push(format!("[image attached @{path}: sent as an image part]"));
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

/// Media type for an image path we can attach, or `None` for anything else.
/// Extensions are only a hint: the bytes are what the wire sends, and this
/// list is the set both provider wires accept.
pub fn image_media_type(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        _ => None,
    }
}

/// Largest image sent to a provider (base64 makes it ~33% bigger on the wire).
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Collect the image `@paths` in a task as message parts.
///
/// Returns `(images, notes)`: `notes` are one-line, human-facing explanations
/// for anything that could not be attached (missing, too large, empty), so the
/// user learns about it instead of silently losing the attachment.
pub fn expand_image_refs(
    task: &str,
    cwd: &std::path::Path,
) -> (Vec<crate::llm::ImagePart>, Vec<String>) {
    let mut images: Vec<crate::llm::ImagePart> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for raw in task.split_whitespace() {
        let Some(path) = ref_path(raw) else {
            continue;
        };
        let Some(media_type) = image_media_type(path) else {
            continue;
        };
        let full = cwd.join(path);
        match std::fs::metadata(&full) {
            Ok(meta) if meta.len() > MAX_IMAGE_BYTES => notes.push(format!(
                "@{path} is {} bytes, over the {} MiB image limit — not attached",
                meta.len(),
                MAX_IMAGE_BYTES / 1024 / 1024
            )),
            Ok(meta) if meta.len() == 0 => {
                notes.push(format!("@{path} is empty — not attached"));
            }
            Ok(_) => match std::fs::read(&full) {
                Ok(bytes) => images.push(crate::llm::ImagePart::new(media_type, &bytes)),
                Err(e) => notes.push(format!("cannot read @{path}: {e}")),
            },
            Err(_) => notes.push(format!("@{path} does not exist — not attached")),
        }
    }
    (images, notes)
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

/// Read a workspace file for diffing: text only, size-capped, silently absent
/// when the file does not exist (a new file diffs against "").
fn read_text_for_diff(cwd: &std::path::Path, rel: &str) -> Option<String> {
    let path = cwd.join(rel);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_DIFF_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Unified diff produced by one successful edit call, if the file exists and
/// its content actually changed.
fn diff_for(cwd: &std::path::Path, call: &ToolCall, before: Option<&str>) -> Option<String> {
    let rel = crate::tools::edited_path(call)?;
    let after = read_text_for_diff(cwd, rel)?;
    crate::diff::unified(before.unwrap_or(""), &after, rel, 3).map(|d| d.text)
}

/// Tool text embedded in an event, capped (the model still sees everything).
fn event_output(text: &str) -> String {
    if text.chars().count() <= MAX_EVENT_OUTPUT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_EVENT_OUTPUT).collect();
    out.push_str("\n...[event truncated]");
    out
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
        // `derive` gives the sub-agent its OWN permission cell: a read-only
        // sub-task must not rewrite the parent session's mode (it used to).
        let mode = if read_only {
            crate::tools::PermissionMode::ReadOnly
        } else {
            self.base_ctx.permission.get()
        };
        let mut ctx = self.base_ctx.derive(mode);
        ctx.subagent = None; // delegation cannot nest
        ctx.checkpointing = None; // sub-agent edits are not a /undo epoch
        ctx.todos = None; // its plan is its own
        ctx.staged = None; // staging belongs to the main conversation's review
                           // Its prompts are labelled so the human never confuses a nested
                           // approval with their own task.
        ctx.approver = crate::tools::subagent::scope_approver(&ctx.approver);
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
         - Independent tool calls belong in the SAME round (they run in parallel);\n\
         only sequence a call when it really needs an earlier result.\n\
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
    /// Images were attached but the provider is marked `vision = false`.
    #[error(
        "the provider `{nick}` is configured with vision = false, so the {count} attached image(s) cannot be sent — remove them, or set `vision = true` / drop the flag in its [providers.*] entry"
    )]
    VisionUnsupported { nick: String, count: usize },
    /// The per-task USD budget was reached between rounds. History is kept,
    /// so raising the budget and asking again resumes where it stopped.
    #[error(
        "task stopped: spent ${spent:.4} of the ${budget:.2} per-task budget (agent.max_usd_per_task)"
    )]
    BudgetExceeded { spent: f64, budget: f64 },
}

/// What the UI wants to know while a task runs.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// Assistant text fragment, in order.
    TextDelta { text: String },
    /// A tool call is starting. `id` matches the wire call id, so parallel
    /// calls never get their cards crossed; `summary` is a one-line digest of
    /// the arguments (path, command, query...).
    ToolStart {
        id: String,
        name: String,
        summary: String,
    },
    /// The model's plan checklist changed: `(content, status)` pairs, in
    /// order. Emitted after a tool round when `todo_write` ran.
    TodosUpdated { items: Vec<(String, String)> },
    /// Tool call finished: error flag, one-line preview, the full (capped)
    /// output for an expanded UI card, an optional unified diff when the tool
    /// rewrote a file, and how long it took.
    ToolEnd {
        id: String,
        name: String,
        is_error: bool,
        preview: String,
        /// Full tool text, capped at [`MAX_EVENT_OUTPUT`] chars.
        output: String,
        /// Unified diff of the file this call rewrote (`write_file`,
        /// `apply_patch`), absent otherwise or when nothing changed.
        diff: Option<String>,
        elapsed_ms: u64,
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
        /// Task cost so far, absent when the model has no price entry.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    /// Review mode: a round staged its writes and the changeset is waiting for
    /// a verdict. `files` are display paths with their `+added/-removed`
    /// counts, so a UI can render the summary without holding the agent.
    ChangesetPending { files: Vec<(String, usize, usize)> },
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
    /// Active effort preset name (`fast` | `balanced` | `deep` | custom),
    /// shown in the header and carried across provider rebuilds.
    preset: String,
    /// USD budget for one task (`agent.max_usd_per_task`); `None` = no guard.
    max_usd_per_task: Option<f64>,
    /// Price entry for the active model (`[usage.prices]`), if configured.
    /// Absent = the UI shows tokens and never invents money.
    price: Option<crate::config::PriceCfg>,
    /// USD spent by the CURRENT task (0 when the model has no price entry).
    cost_usd: f64,
    /// Whether `cost_usd` means anything (a priced round happened).
    priced: bool,
    /// `[providers.<nick>] vision`: `Some(false)` refuses images before the
    /// request; `None` lets the provider decide.
    vision: Option<bool>,
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
            preset: "balanced".to_string(),
            max_usd_per_task: None,
            price: None,
            cost_usd: 0.0,
            priced: false,
            vision: None,
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

    /// Active effort preset (`/effort`, header badge).
    pub fn preset(&self) -> &str {
        &self.preset
    }

    /// Repoint the preset label (the provider already carries the tuning it
    /// was built with; this keeps the header and `/status` honest).
    pub fn set_preset(&mut self, name: String) {
        self.preset = name;
    }

    /// Per-task USD budget, when configured.
    pub fn max_usd_per_task(&self) -> Option<f64> {
        self.max_usd_per_task
    }

    /// Model id the provider was built for (pricing key).
    pub fn provider_model(&self) -> String {
        self.provider
            .describe()
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }

    /// USD spent by the current task, when the model has a price entry.
    pub fn cost_usd(&self) -> Option<f64> {
        self.priced.then_some(self.cost_usd)
    }

    /// Replace the price entry (a rebuilt agent keeps its own model).
    pub fn set_price(&mut self, price: Option<crate::config::PriceCfg>) {
        self.price = price;
        self.cost_usd = 0.0;
        self.priced = false;
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
        agent.preset = cfg.active_preset().0;
        agent.max_usd_per_task = cfg.agent.max_usd_per_task;
        agent.price = cfg.price_for(&agent.provider_model()).cloned();
        agent.vision = cfg.providers.get(&agent.nick).and_then(|e| e.vision);
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
                let labelled = if summary.contains("FILES:") {
                    summary
                } else {
                    // a model that ignored the shape still gets a label, so the
                    // UI can say the summary is unstructured
                    format!("[unstructured summary]\n{summary}")
                };
                self.history.insert_summary(labelled);
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

    /// Fixed-shape summarizer prompt: a free-form summary is hard to trust and
    /// impossible to scan; four named sections keep the facts the next round
    /// actually needs (paths, decisions, open work, next step).
    const SUMMARY_PROMPT: &'static str = "Compress the following agent conversation transcript for a compacted context window. Reply in EXACTLY this shape, terse bullets, no prose:\nGOAL: one line\nFILES: every path touched or discussed, with what happened to it\nDECISIONS: choices made and why (one line each)\nOPEN: unfinished work or unanswered questions\nNEXT: the single next step";

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
        on_event: impl FnMut(AgentEvent),
    ) -> Result<String, AgentError> {
        self.run_task_with_images(task, Vec::new(), on_event).await
    }

    /// Same, with images attached to the task's user message.
    ///
    /// A provider explicitly marked `vision = false` is refused BEFORE the
    /// request: a certain provider rejection must not cost a round (or the
    /// attachment's tokens).
    pub async fn run_task_with_images(
        &mut self,
        task: &str,
        images: Vec<crate::llm::ImagePart>,
        mut on_event: impl FnMut(AgentEvent),
    ) -> Result<String, AgentError> {
        if !images.is_empty() && self.vision == Some(false) {
            return Err(AgentError::VisionUnsupported {
                nick: self.nick.clone(),
                count: images.len(),
            });
        }
        self.history
            .push(Message::user_with_images(task.to_string(), images));
        self.usage = Usage::default();
        self.cost_usd = 0.0;
        self.priced = false;
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
            if let Some(price) = &self.price {
                if let Some(cost) = price.cost(&resp.usage) {
                    self.cost_usd += cost;
                    self.priced = true;
                }
            }
            on_event(AgentEvent::RoundDone {
                round: round + 1,
                usage: self.usage.clone(),
                est_tokens: self.history.estimate_tokens(),
                cost_usd: self.cost_usd(),
            });
            // agent.max_usd_per_task: stop between rounds, keeping the history
            // (the work so far is still usable) instead of blowing a budget.
            if let (Some(budget), Some(spent)) = (self.max_usd_per_task, self.cost_usd()) {
                if spent >= budget {
                    return Err(AgentError::BudgetExceeded { spent, budget });
                }
            }

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

            let mut verdicts: Vec<Option<(ToolOutcome, u64)>> = vec![None; calls.len()];
            // Pre-edit content of every file this round is about to rewrite.
            // Read before dispatch (and before approval) so the UI can show a
            // real diff instead of just "wrote N bytes".
            let before: Vec<Option<String>> = calls
                .iter()
                .map(|c| {
                    crate::tools::edited_path(c)
                        .and_then(|rel| read_text_for_diff(&self.ctx.cwd, rel))
                })
                .collect();
            let mut pending: Vec<(usize, ToolCall)> = Vec::new();
            for (i, call) in calls.iter().enumerate() {
                if let Some(summary) = self.registry.approval_summary(call, &self.ctx) {
                    on_event(AgentEvent::ToolStart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        summary: crate::tools::call_summary(call),
                    });
                    match self.ctx.approver.decide(&call.name, &summary).await {
                        crate::tools::Decision::Deny(reason) => {
                            let denied = ToolOutcome::err(crate::tools::shell::command_denied(
                                reason.as_deref(),
                            ));
                            on_event(AgentEvent::ToolEnd {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                is_error: true,
                                preview: denied.first_line_preview(160),
                                output: event_output(&denied.text),
                                diff: None,
                                elapsed_ms: 0,
                            });
                            verdicts[i] = Some((denied, 0));
                        }
                        // Allow / AllowSession (the approver already recorded
                        // session memory) both run the call now.
                        _ => pending.push((i, call.clone())),
                    }
                } else {
                    on_event(AgentEvent::ToolStart {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        summary: crate::tools::call_summary(call),
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
                    let started = std::time::Instant::now();
                    let outcome = registry.execute(&call, &ctx).await;
                    (i, outcome, started.elapsed().as_millis() as u64)
                });
            }
            // Esc must bite while a tool runs too: poll the flag and abort the
            // tool tasks (spawned commands are kill_on_drop, so a hung
            // `run_command` dies with its task instead of holding the run).
            let mut remaining = set.len();
            while remaining > 0 {
                tokio::select! {
                    joined = set.join_next() => match joined {
                        Some(Ok((i, outcome, elapsed_ms))) => {
                            let diff = if outcome.is_error {
                                None
                            } else {
                                diff_for(&self.ctx.cwd, &calls[i], before[i].as_deref())
                            };
                            on_event(AgentEvent::ToolEnd {
                                id: calls[i].id.clone(),
                                name: calls[i].name.clone(),
                                is_error: outcome.is_error,
                                preview: outcome.first_line_preview(160),
                                output: event_output(&outcome.text),
                                diff,
                                elapsed_ms,
                            });
                            verdicts[i] = Some((outcome, elapsed_ms));
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

            // ---- review mode ------------------------------------------
            // Write tools parked their content instead of touching the disk.
            // Show the changeset, take a per-file verdict, write only what was
            // accepted, then rewrite each staged call's tool result so the
            // model learns which of its writes actually happened.
            if let (Some(staging), Some(reviewer)) =
                (self.ctx.staged.as_ref(), self.ctx.reviewer.as_ref())
            {
                if !staging.is_empty() && self.ctx.reviewing() {
                    let changes = staging.take();
                    let summary: Vec<(String, usize, usize)> = changes
                        .iter()
                        .map(|c| {
                            let (added, removed) = c.counts(&self.ctx.cwd);
                            (c.display_path(&self.ctx.cwd), added, removed)
                        })
                        .collect();
                    on_event(AgentEvent::ChangesetPending { files: summary });
                    let outcome = reviewer.review(&self.ctx.cwd, &changes).await;
                    let results = crate::tools::apply_reviewed(
                        &changes,
                        &outcome,
                        self.ctx.checkpointing.as_ref().map(|c| c.as_ref()),
                    );
                    // The note (if any) rides inside each rejected tool result:
                    // a user message inserted here would sit between the
                    // assistant's tool_calls and their results, which the
                    // Anthropic wire rejects outright.
                    let note_suffix = outcome
                        .note
                        .as_deref()
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default();
                    // map path -> verdict text, then attach to the calls that
                    // produced each staged change (last write wins per path)
                    for (change, result) in changes.iter().zip(results) {
                        let is_error = result.is_err();
                        let text = match result {
                            Ok(()) => format!(
                                "reviewed + written: {} ({} bytes)",
                                change.display_path(&self.ctx.cwd),
                                change.after.len()
                            ),
                            Err(reason) => format!(
                                "NOT written: {} — {reason}{note_suffix}. Keep the rest of the plan; do not retry this file unless the user asks.",
                                change.display_path(&self.ctx.cwd)
                            ),
                        };
                        let targets: Vec<usize> = calls
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| {
                                crate::tools::edited_path(c)
                                    .map(|p| self.ctx.cwd.join(p) == change.path)
                                    .unwrap_or(false)
                            })
                            .map(|(i, _)| i)
                            .collect();
                        for i in targets {
                            let (outcome_text, _) =
                                std::mem::take(&mut verdicts[i]).expect("outcome exists");
                            let _ = outcome_text;
                            verdicts[i] = Some((
                                crate::tools::ToolOutcome {
                                    text: text.clone(),
                                    is_error,
                                },
                                0,
                            ));
                        }
                    }
                }
            }

            // did this round really change files? Computed AFTER review, so a
            // rejected changeset does not trigger the post-edit feedback run.
            let wrote = calls.iter().enumerate().any(|(i, c)| {
                crate::tools::is_edit_tool(&c.name)
                    && verdicts[i]
                        .as_ref()
                        .map(|(o, _)| !o.is_error)
                        .unwrap_or(false)
            });

            for (i, call) in calls.iter().enumerate() {
                let (outcome, _) =
                    std::mem::take(&mut verdicts[i]).expect("every call gets an outcome");
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
            staged: None,
            reviewer: None,
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

    #[test]
    fn file_ref_names_lists_dedupes_and_strips_punctuation() {
        assert_eq!(
            file_ref_names("compare @a.txt, @b.rs and @a.txt again"),
            vec!["a.txt".to_string(), "b.rs".to_string()]
        );
        assert_eq!(
            file_ref_names("email me at @home."),
            vec!["home".to_string()]
        );
        assert!(file_ref_names("no refs here").is_empty());
        assert!(file_ref_names("@").is_empty());
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
            .any(|e| matches!(e, AgentEvent::ToolStart { ref name, .. } if name == "read_file")));
        assert!(evs.iter().any(
            |e| matches!(e, AgentEvent::ToolEnd { is_error: false, ref name, .. } if name == "read_file")
        ));
        assert!(evs
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text.contains("note says"))));
    }

    #[tokio::test]
    async fn edit_events_carry_summary_output_and_diff() {
        let dir = scratch_named("diff-round");
        std::fs::write(dir.join("x.txt"), "v1\nkeep\n").unwrap();
        let script = vec![
            LlmResponse {
                tool_calls: vec![ToolCall {
                    id: "e1".into(),
                    name: "write_file".into(),
                    arguments: json!({"path": "x.txt", "content": "v2\nkeep\n"}),
                }],
                stop_reason: Some("tool_use".into()),
                usage: Usage::default(),
                content: None,
            },
            LlmResponse {
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let mut ctx = test_ctx();
        ctx.cwd = dir.clone();
        let mut a = agent_with_ctx(script, ctx);
        let events = Arc::new(std::sync::Mutex::new(Vec::<AgentEvent>::new()));
        let sink = events.clone();
        a.run_task("rewrite x.txt", move |e| sink.lock().unwrap().push(e))
            .await
            .unwrap();
        let evs = events.lock().unwrap();
        assert!(
            evs.iter().any(|e| matches!(
                e,
                AgentEvent::ToolStart { id, name, summary }
                    if id == "e1" && name == "write_file" && summary.contains("x.txt")
            )),
            "start event: {evs:?}"
        );
        let end = evs
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolEnd {
                    id,
                    output,
                    diff,
                    is_error,
                    elapsed_ms,
                    ..
                } if id == "e1" => Some((output, diff, is_error, elapsed_ms)),
                _ => None,
            })
            .expect("tool end event");
        assert!(!*end.2, "the write succeeded");
        assert!(end.0.contains("wrote"), "output: {}", end.0);
        let diff = end.1.as_deref().expect("a diff for the rewritten file");
        assert!(diff.contains("-v1"), "diff: {diff}");
        assert!(diff.contains("+v2"), "diff: {diff}");
        assert!(diff.contains(" keep"), "context line: {diff}");
        let _ = *end.3; // duration is recorded (may be 0 on a fast disk)
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
                    ..Default::default()
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
                    ..Default::default()
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
            staged: None,
            reviewer: None,
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

    #[tokio::test]
    async fn priced_rounds_accumulate_and_the_budget_stops_the_task() {
        let script = vec![
            LlmResponse {
                usage: Usage {
                    input_tokens: Some(1_000_000),
                    output_tokens: Some(0),
                    ..Default::default()
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
                    input_tokens: Some(1_000_000),
                    output_tokens: Some(0),
                    ..Default::default()
                },
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let mut a = agent_with(script);
        a.set_price(Some(crate::config::PriceCfg {
            input: Some(1.0), // $1 / 1M tokens
            output: Some(0.0),
            cache_read: None,
            cache_write: None,
        }));
        // $1 per round, $1.5 budget: the first round fits, the second does not
        a.max_usd_per_task = Some(1.5);
        let err = a.run_task("go", |_| {}).await.unwrap_err();
        match err {
            AgentError::BudgetExceeded { spent, budget } => {
                assert!((spent - 2.0).abs() < 1e-9, "{spent}");
                assert_eq!(budget, 1.5);
            }
            other => panic!("expected the budget error, got {other}"),
        }
        // no price -> no guard, no invented money
        let mut b = agent_with(vec![LlmResponse {
            content: Some("free".into()),
            ..Default::default()
        }]);
        b.max_usd_per_task = Some(0.0);
        assert!(b.run_task("go", |_| {}).await.is_ok());
        assert_eq!(b.cost_usd(), None);
    }

    #[tokio::test]
    async fn review_mode_stages_instead_of_writing() {
        use crate::tools::{ReviewOutcome, ReviewedFile, Reviewer};
        use std::sync::Mutex;

        // a reviewer that rejects exactly one path and keeps a copy of what it
        // was shown, so the test can assert the model sees a real diff
        struct Picky {
            reject: std::path::PathBuf,
            seen: Arc<Mutex<Vec<(String, String)>>>,
        }
        #[async_trait::async_trait]
        impl Reviewer for Picky {
            async fn review(
                &self,
                cwd: &std::path::Path,
                changes: &[crate::tools::checkpoint::StagedChange],
            ) -> ReviewOutcome {
                let mut seen = self.seen.lock().unwrap();
                for c in changes {
                    let diff = c.diff(cwd).map(|d| d.text).unwrap_or_default();
                    seen.push((c.display_path(cwd), diff));
                }
                ReviewOutcome {
                    files: changes
                        .iter()
                        .map(|c| ReviewedFile {
                            path: c.path.clone(),
                            accepted: c.path != self.reject,
                        })
                        .collect(),
                    note: None,
                }
            }
        }

        let dir = std::env::temp_dir().join(format!("kazi-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kept = dir.join("keep.txt");
        let dropped = dir.join("drop.txt");

        let calls = vec![
            LlmResponse {
                tool_calls: vec![
                    ToolCall {
                        id: "w1".into(),
                        name: "write_file".into(),
                        arguments: json!({"path": "keep.txt", "content": "kept content"}),
                    },
                    ToolCall {
                        id: "w2".into(),
                        name: "write_file".into(),
                        arguments: json!({"path": "drop.txt", "content": "dropped content"}),
                    },
                ],
                ..Default::default()
            },
            LlmResponse {
                content: Some("done".into()),
                ..Default::default()
            },
        ];
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(calls));
        let mut ctx = test_ctx();
        ctx.cwd = dir.clone();
        ctx.permission.set(crate::tools::PermissionMode::Review);
        ctx.staged = Some(Arc::new(crate::tools::checkpoint::Staging::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        ctx.reviewer = Some(Arc::new(Picky {
            reject: dropped.clone(),
            seen: seen.clone(),
        }));
        let mut agent = Agent::with_parallel(
            provider,
            ToolRegistry::builtin(),
            ctx,
            Some("sys".into()),
            4,
            10_000,
            2,
        );
        agent.run_task("write two files", |_| {}).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "kept content",
            "the accepted file is written"
        );
        assert!(
            !dropped.exists(),
            "the rejected file must never touch the disk"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the reviewer saw both staged files");
        assert!(
            seen.iter()
                .any(|(p, d)| p == "drop.txt" && d.contains("+dropped content")),
            "the review shows a real diff: {seen:?}"
        );
        // the model is told which write did not happen
        let tool_text: Vec<String> = agent
            .history
            .msgs
            .iter()
            .filter(|m| m.role == crate::llm::Role::Tool)
            .map(|m| m.text().to_string())
            .collect();
        assert!(
            tool_text
                .iter()
                .any(|t| t.contains("NOT written: drop.txt")),
            "{tool_text:?}"
        );
        assert!(
            tool_text
                .iter()
                .any(|t| t.contains("reviewed + written: keep.txt")),
            "{tool_text:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn images_are_attached_or_refused_before_the_request() {
        let dir = std::env::temp_dir().join(format!("kazi-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shot.png"), b"PNGDATA").unwrap();
        std::fs::write(dir.join("note.txt"), "text").unwrap();

        // images and text refs are separated: png -> part, txt -> inline text
        let task = "look at @shot.png and @note.txt";
        let (images, notes) = expand_image_refs(task, &dir);
        assert_eq!(images.len(), 1);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(images[0].media_type, "image/png");
        let expanded = expand_file_refs(task, &dir);
        assert!(expanded.contains("note.txt"), "text ref inlined");
        assert!(expanded.contains("image attached @shot.png"));

        // a missing image is reported, not silently skipped
        let (images, notes) = expand_image_refs("see @nope.png", &dir);
        assert!(images.is_empty());
        assert!(notes[0].contains("does not exist"), "{notes:?}");

        // a provider marked vision = false refuses before any round runs
        let script = vec![LlmResponse {
            content: Some("should not be reached".into()),
            ..Default::default()
        }];
        let mut a = agent_with(script);
        a.vision = Some(false);
        let err = a
            .run_task_with_images("look", images_for(&dir), |_| {})
            .await
            .unwrap_err();
        assert!(matches!(err, AgentError::VisionUnsupported { .. }), "{err}");

        // ... while an unset flag sends them
        let mut b = agent_with(vec![LlmResponse {
            content: Some("seen".into()),
            ..Default::default()
        }]);
        b.vision = None;
        assert!(b
            .run_task_with_images("look", images_for(&dir), |_| {})
            .await
            .is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn images_for(dir: &std::path::Path) -> Vec<crate::llm::ImagePart> {
        let (images, _) = expand_image_refs("@shot.png", dir);
        images
    }

    #[test]
    fn image_media_types_are_a_whitelist() {
        assert_eq!(image_media_type("a.PNG"), Some("image/png"));
        assert_eq!(image_media_type("a.jpeg"), Some("image/jpeg"));
        assert_eq!(image_media_type("a.webp"), Some("image/webp"));
        assert_eq!(image_media_type("a.gif"), Some("image/gif"));
        assert_eq!(image_media_type("a.pdf"), None);
        assert_eq!(image_media_type("noext"), None);
    }

    #[test]
    fn derived_ctx_does_not_touch_the_parent_permission() {
        // The 0.18 bug: `delegate` cloned ToolCtx (which shares the permission
        // atom) and flipped the copy to read-only for a sub-task — silently
        // turning the whole session read-only.
        let parent = test_ctx();
        parent.permission.set(crate::tools::PermissionMode::Edit);
        let sub = parent.derive(crate::tools::PermissionMode::ReadOnly);
        assert_eq!(
            parent.permission.get(),
            crate::tools::PermissionMode::Edit,
            "a derived context must not rewrite its parent's mode"
        );
        assert_eq!(sub.permission.get(), crate::tools::PermissionMode::ReadOnly);
        // the parent keeps every other shared handle (they are meant to be shared)
        assert_eq!(sub.output_limit, parent.output_limit);
    }
}
