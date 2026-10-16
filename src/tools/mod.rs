//! Tool system: the capability surface the model can act through.
//!
//! A [`Tool`] is a named capability with a JSON-Schema description; the model
//! emits calls by name + arguments, and [`ToolRegistry::execute`] runs the
//! matching implementation. Everything a tool reports — including failures —
//! flows back to the model as text (`is_error` marks failures), so the model
//! can adapt instead of the loop aborting.

pub mod checkpoint;
pub mod fetch;
pub mod file;
pub mod patch;
pub mod search;
pub mod shell;
pub mod subagent;
pub mod todo;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::llm::{ToolCall, ToolSpec};

/// Failure value returned by a tool implementation. Errors never abort the
/// agent loop; they become an error tool-result the model can react to.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(err: impl Into<String>) -> Self {
        Self(err.into())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::new(e.to_string())
    }
}

/// What an executed tool produces: text for the model + the error flag.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutcome {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
    pub fn first_line_preview(&self, cap: usize) -> String {
        let mut s: String = self.text.lines().next().unwrap_or("").to_string();
        if s.chars().count() > cap {
            s = s.chars().take(cap).collect();
            s.push_str("...");
        }
        s
    }
}

/// Execution context handed to every tool run; implemented from config.
#[derive(Clone)]
pub struct ToolCtx {
    /// Working directory the user launched kazi from.
    pub cwd: std::path::PathBuf,
    /// If set, file tools may not touch paths outside this directory.
    pub sandbox_root: Option<std::path::PathBuf>,
    pub timeout: Duration,
    /// Max characters of tool output fed back to the model.
    pub output_limit: usize,
    /// `run_command` prefixes auto-approved without prompting.
    pub allow_commands: Vec<String>,
    pub approver: Arc<dyn Approver>,
    /// Write-ahead checkpoints for `/undo` (absent in bare library tests).
    pub checkpointing: Option<Arc<checkpoint::Checkpointing>>,
    /// The model's plan checklist (absent in bare library tests).
    pub todos: Option<Arc<todo::TodoList>>,
    /// How much the agent may do without asking (default: `Edit`).
    pub permission: Permission,
    /// Factory for the `delegate` tool's sub-agents (`None` = tool refuses).
    pub subagent: Option<Arc<dyn subagent::SubAgentRunner>>,
}

/// How much the agent may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Inspection only: write tools and `run_command` refuse outright.
    ReadOnly,
    /// Default: writes run, `run_command` is approval-gated.
    Edit,
    /// Everything runs unattended (`--yolo`).
    Yolo,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" | "readonly" | "read-only" => Some(Self::ReadOnly),
            "edit" | "default" => Some(Self::Edit),
            "yolo" | "auto" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Edit => "edit",
            Self::Yolo => "yolo",
        }
    }

    /// Next mode in the Shift+Tab cycle.
    pub fn cycle(&self) -> Self {
        match self {
            Self::Edit => Self::ReadOnly,
            Self::ReadOnly => Self::Yolo,
            Self::Yolo => Self::Edit,
        }
    }
}

/// Shared, atomically switchable permission mode (TUI Shift+Tab, REPL
/// `/permission`, config at startup all point at the same cell).
#[derive(Clone)]
pub struct Permission {
    mode: Arc<std::sync::atomic::AtomicU8>,
}

impl Default for Permission {
    fn default() -> Self {
        Self::new(PermissionMode::Edit)
    }
}

impl Permission {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode: Arc::new(std::sync::atomic::AtomicU8::new(mode_code(mode))),
        }
    }

    pub fn get(&self) -> PermissionMode {
        match self.mode.load(std::sync::atomic::Ordering::Relaxed) {
            0 => PermissionMode::ReadOnly,
            2 => PermissionMode::Yolo,
            _ => PermissionMode::Edit,
        }
    }

    pub fn set(&self, mode: PermissionMode) {
        self.mode
            .store(mode_code(mode), std::sync::atomic::Ordering::Relaxed);
    }
}

fn mode_code(m: PermissionMode) -> u8 {
    match m {
        PermissionMode::ReadOnly => 0,
        PermissionMode::Edit => 1,
        PermissionMode::Yolo => 2,
    }
}

/// One approval answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run it this once.
    Allow,
    /// Run it and remember the approval for the rest of the session
    /// (implementations key that on [`approval_key`]).
    AllowSession,
    /// Refuse; the model gets an error tool-result.
    Deny,
}

/// Stable per-command key used for session-scoped approval memory:
/// the first token of the summary (e.g. `$ git` → `git`).
pub fn approval_key(summary: &str) -> String {
    summary
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('$')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string()
}

/// Interactive gate for potentially-impactful actions (`run_command`).
/// Implementations: terminal prompt (CLI/REPL), TUI prompt over a channel,
/// always-allow (`--yolo`), always-deny (library default), tests.
#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, tool: &str, summary: &str) -> Decision;
}

/// Always allows. Only used with the explicit `--yolo` flag.
pub struct AutoApprover;

#[async_trait]
impl Approver for AutoApprover {
    async fn decide(&self, _tool: &str, _summary: &str) -> Decision {
        Decision::Allow
    }
}

/// Always denies (documented safe default for library use).
pub struct DenyApprover;

#[async_trait]
impl Approver for DenyApprover {
    async fn decide(&self, _tool: &str, _summary: &str) -> Decision {
        Decision::Deny
    }
}

/// Standard [`ToolCtx`] derived from the `[tools]` config section, the launch
/// directory, an approver supplied by the caller (CLI injects the terminal
/// prompt or `AutoApprover` with `--yolo`), and the startup permission mode.
pub fn tool_ctx(
    cfg: &crate::config::ToolsCfg,
    cwd: &std::path::Path,
    approver: std::sync::Arc<dyn Approver>,
    mode: PermissionMode,
) -> ToolCtx {
    ToolCtx {
        cwd: cwd.to_path_buf(),
        sandbox_root: cfg.sandbox_root.as_ref().map(|s| {
            let p = std::path::PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        }),
        timeout: Duration::from_secs(cfg.command_timeout_secs.unwrap_or(120)),
        output_limit: cfg.output_limit.unwrap_or(64_000),
        allow_commands: cfg.allow_commands.clone().unwrap_or_default(),
        approver,
        // write-ahead checkpoints live under the platform data dir
        checkpointing: dirs::data_dir().map(|d| {
            Arc::new(checkpoint::Checkpointing::new(
                d.join("kazi").join("checkpoints"),
            ))
        }),
        todos: Some(Arc::new(todo::TodoList::default())),
        permission: Permission::new(mode),
        subagent: None, // the agent wires its own runner in
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    /// Exposed tool name (unique per registry). Static for built-ins,
    /// dynamic for MCP-served ones (`mcp_<server>_<name>`).
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON-Schema `{"type":"object","properties":{...},"required":[...]}`.
    /// Kept hand-written (not codegen) so each tool documents its own contract
    /// in one visible place.
    fn parameters(&self) -> Value;
    /// Pre-flight gate: return `Some(summary)` when executing this call needs
    /// explicit approval. The engine asks the [`Approver`] ONCE per call,
    /// serially, before dispatching — so parallel rounds never interleave
    /// prompts, and `run()` can assume it was approved.
    /// `None` = no gate (default).
    fn approval_summary(&self, _args: &Value, _ctx: &ToolCtx) -> Option<String> {
        None
    }
    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError>;
}

/// Fetch a string argument from a tool-call arguments object; models are not
/// always schema-perfect, so callers get a precise error back.
pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::new(format!("missing or non-string argument `{key}`")))
}

pub(crate) fn optional_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub(crate) fn optional_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

/// Registry of built-in tools; names are unique keys into the catalog.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Standard toolset shipped with kazi.
    pub fn builtin() -> Self {
        Self::new()
            .register(Arc::new(todo::TodoWrite))
            .register(Arc::new(subagent::Delegate))
            .register(Arc::new(file::ListDir))
            .register(Arc::new(file::ReadFile))
            .register(Arc::new(search::SearchFiles))
            .register(Arc::new(patch::ApplyPatch))
            .register(Arc::new(file::WriteFile))
            .register(Arc::new(shell::RunCommand))
            .register(Arc::new(fetch::FetchUrl))
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Tool catalog as JSON spec list for the current model round.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// One-line-per-tool catalog listing (for logs/UI).
    pub fn catalog(&self) -> String {
        self.tools
            .iter()
            .map(|t| format!("{}: {}", t.name(), t.description()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Engine-side pre-flight: `Some(summary)` when this call needs explicit
    /// user approval (see [`Tool::approval_summary`]).
    pub fn approval_summary(&self, call: &ToolCall, ctx: &ToolCtx) -> Option<String> {
        let tool = self.tools.iter().find(|t| t.name() == call.name)?;
        tool.approval_summary(&call.arguments, ctx)
    }

    /// Execute a model-requested tool call. Infallible at this level — errors
    /// are expressed as error-outcomes so the loop continues.
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolOutcome {
        let tool = match self.tools.iter().find(|t| t.name() == call.name) {
            Some(t) => t,
            None => {
                return ToolOutcome::err(format!(
                    "unknown tool `{}`; available: {}",
                    call.name,
                    self.names().join(", ")
                ));
            }
        };
        match tool.run(&call.arguments, ctx).await {
            Ok(text) => ToolOutcome::ok(text),
            Err(e) => ToolOutcome::err(e.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use serde_json::json;

    #[test]
    fn builtin_registry_spec_shapes() {
        let reg = ToolRegistry::builtin();
        let names = reg.names();
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "todo_write",
                "delegate",
                "list_dir",
                "read_file",
                "search_files",
                "apply_patch",
                "write_file",
                "run_command",
                "fetch_url"
            ]
        );
        for spec in reg.specs() {
            assert_eq!(
                spec.parameters["type"], "object",
                "{} must declare object schema",
                spec.name
            );
            assert!(spec.parameters["properties"].is_object());
        }
    }

    #[test]
    fn permission_mode_parse_and_cycle() {
        use PermissionMode::*;
        assert_eq!(PermissionMode::parse("read-only"), Some(ReadOnly));
        assert_eq!(PermissionMode::parse(" READ "), Some(ReadOnly));
        assert_eq!(PermissionMode::parse("yolo"), Some(Yolo));
        assert_eq!(PermissionMode::parse("edit"), Some(Edit));
        assert_eq!(PermissionMode::parse("nope"), None);
        assert_eq!(Edit.cycle(), ReadOnly);
        assert_eq!(ReadOnly.cycle(), Yolo);
        assert_eq!(Yolo.cycle(), Edit);

        let p = Permission::default();
        assert_eq!(p.get(), Edit);
        let cloned = p.clone();
        p.set(Yolo);
        assert_eq!(cloned.get(), Yolo, "clones share the same cell");
    }

    #[test]
    fn approval_key_takes_first_command_token() {
        assert_eq!(approval_key("$ git status --short\n  cwd: /tmp"), "git");
        assert_eq!(approval_key(" $ cargo test"), "cargo");
        assert_eq!(approval_key(""), "");
    }

    #[test]
    fn unknown_tool_yields_error_outcome() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let reg = ToolRegistry::new().register(Arc::new(file::ListDir));
            let ctx = test_ctx();
            let out = reg
                .execute(
                    &ToolCall {
                        id: "1".into(),
                        name: "nope".into(),
                        arguments: json!({}),
                    },
                    &ctx,
                )
                .await;
            assert!(out.is_error);
            assert!(out.text.contains("unknown tool"));
        });
    }

    /// Small helper reused by tool-level tests.
    pub(crate) fn test_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::current_dir().unwrap(),
            sandbox_root: None,
            timeout: Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec!["echo".into()],
            approver: Arc::new(AutoApprover),
            checkpointing: None,
            todos: None,
            permission: Permission::default(),
            subagent: None,
        }
    }
}
