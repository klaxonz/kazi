//! `delegate` — hand a self-contained sub-task to a fresh agent context and
//! get a summary back.
//!
//! The sub-agent runs on the same provider and working-directory rules but
//! with its own (short) history, so a long exploration does not consume the
//! main conversation's context. `read_only` (the default) pins the sub-agent
//! to inspection; without it the sub-agent inherits the main approver, so any
//! command it wants to run still asks the user.
//!
//! Nesting is prevented by construction: the runner clears
//! [`super::ToolCtx::subagent`] on the context it hands down.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{optional_str, Tool, ToolCtx, ToolError};

/// Label put on a sub-agent's approval summary, so a nested prompt can never be
/// mistaken for the user's own task.
pub const SUBAGENT_MARK: &str = "[sub-agent] ";

/// Give a sub-agent an approver that reports its origin. Approvers that cannot
/// be scoped (test doubles) are shared unchanged.
pub fn scope_approver(
    approver: &std::sync::Arc<dyn super::Approver>,
) -> std::sync::Arc<dyn super::Approver> {
    approver
        .scoped("sub-agent")
        .unwrap_or_else(|| approver.clone())
}

/// What a sub-agent run produces for the calling tool.
#[async_trait]
pub trait SubAgentRunner: Send + Sync {
    async fn run_sub(&self, prompt: &str, read_only: bool) -> Result<String, String>;
}

pub struct Delegate;
// probe

#[async_trait]
impl Tool for Delegate {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Run a separate agent on a self-contained sub-task and get only its final summary back. Use it to explore a large area, or to do an isolated piece of work, without filling up your own context. Give a complete, standalone prompt (the sub-agent cannot see this conversation) and ask for a concise report of findings, file paths, and what was changed."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Complete, standalone instructions for the sub-agent"
                },
                "read_only": {
                    "type": "boolean",
                    "description": "true (default): the sub-agent may only inspect. false: it may also edit/run commands (still approval-gated)."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let prompt = super::arg_str(args, "prompt")?;
        if prompt.trim().is_empty() {
            return Err(ToolError::new("argument `prompt` must not be empty"));
        }
        let read_only = args
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let _ = optional_str(args, "read_only"); // schema drift tolerance
        let Some(runner) = &ctx.subagent else {
            return Err(ToolError::new(
                "sub-agents are not available in this configuration",
            ));
        };
        runner
            .run_sub(prompt, read_only)
            .await
            .map_err(ToolError::new)
    }
}
