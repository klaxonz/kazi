//! `run_command` — the impactful tool. Policy, in order:
//!
//! 1. **Allowlist** — commands whose *exact prefix* matches a configured
//!    `tools.allow_commands` entry AND that contain no shell separators
//!    (`; & | < > backtick`, newline) run without prompting.
//! 2. **Prompt** — otherwise the user confirms via the [`Approver`]; in
//!    headless mode the CLI denies unless `--yolo` was passed.
//! 3. Denials are reported as tool errors, so the model adapts instead of
//!    retrying the same call.
//!
//! Safety notes (documented in README too): this runs a real shell. The
//! separator check is heuristic — allowlist entries should be commands you
//! trust end-to-end. Every execution is visible in the terminal.

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::process::Command;

use super::{arg_str, optional_str, Tool, ToolCtx, ToolError};

pub struct RunCommand;

/// Text handed back to the model when the user denies a command. It lives in
/// the engine's pre-flight now — the model gets it as an error tool-result.
pub const COMMAND_DENIED: &str =
    "the user declined to run this command. Do not retry it; acknowledge and propose a different approach.";

/// True when an allowlist entry covers this command *and* the command cannot
/// smuggle a second statement through shell metacharacters.
pub fn is_auto_approved(cmd: &str, allow: &[String]) -> bool {
    let t = cmd.trim();
    if t.is_empty() {
        return false;
    }
    let smuggleable = t
        .chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '`' | '\n' | '<' | '>'));
    if smuggleable {
        return false;
    }
    allow.iter().any(|a| {
        let a = a.trim();
        a == t || t.starts_with(&format!("{a} "))
    })
}

#[async_trait]
impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }
    fn description(&self) -> &str {
        "Execute a shell command (Windows `cmd /C`, Unix `sh -c`). Non-allowlisted commands require user approval; a declined command means: do not retry it, propose another way. Do not use for plain file operations your tools cover."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Full command line, e.g. `cargo test --lib`"},
                "cwd": {"type": "string", "description": "Optional working directory (default: launch directory)"}
            },
            "required": ["command"]
        })
    }

    /// Gate for the engine's serial pre-flight: allowlisted (and
    /// metacharacter-free) commands run without asking, everything else
    /// produces a one-screen summary the user confirms. `yolo` skips the
    /// gate entirely; read-only mode refuses commands before this point.
    fn approval_summary(&self, args: &Value, ctx: &ToolCtx) -> Option<String> {
        if ctx.permission.get() == crate::tools::PermissionMode::Yolo
            || ctx.permission.get() == crate::tools::PermissionMode::ReadOnly
        {
            return None;
        }
        let Some(cmd) = args.get("command").and_then(Value::as_str) else {
            return None; // malformed call; run() will reject with a precise error
        };
        let cmd = cmd.trim();
        if is_auto_approved(cmd, &ctx.allow_commands) {
            return None;
        }
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(|d| ctx.cwd.join(d))
            .unwrap_or_else(|| ctx.cwd.clone());
        Some(format!("$ {cmd}\n  cwd: {}", cwd.display()))
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        if ctx.permission.get() == crate::tools::PermissionMode::ReadOnly {
            return Err(ToolError::new(
                "run_command is blocked in read-only permission mode (Shift+Tab / /permission to change)",
            ));
        }
        let cmd = arg_str(args, "command")?.trim().to_string();
        if cmd.is_empty() {
            return Err(ToolError::new("argument `command` must not be empty"));
        }
        let dir = match optional_str(args, "cwd") {
            Some(d) => {
                let p = ctx.cwd.join(d);
                if !p.is_dir() {
                    return Err(ToolError::new(format!("cwd {p:?} is not a directory")));
                }
                p
            }
            None => ctx.cwd.clone(),
        };
        // NOTE: approval already happened (engine pre-flight / allowlist).
        let mut c = build_command(&cmd, &dir);
        c.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        let started = std::time::Instant::now();
        let child = c
            .spawn()
            .map_err(|e| ToolError::new(format!("cannot start `{cmd}`: {e}")))?;
        let output = match tokio::time::timeout(ctx.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(ToolError::new(format!("command failed: {e}"))),
            Err(_) => {
                return Err(ToolError::new(format!(
                    "command timed out after {}s (tools.command_timeout_secs); split it into smaller steps",
                    ctx.timeout.as_secs()
                )));
            }
        };
        let duration_ms = started.elapsed().as_millis();

        let half = ctx.output_limit.max(2) / 2;
        let stdout = truncate_lossy(&output.stdout, half);
        let stderr = truncate_lossy(&output.stderr, half);
        let code = output.status.code().unwrap_or(-1);
        let body = format!(
            "--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\n--- end ({duration_ms} ms) ---"
        );
        // Non-zero exits are tool *results* with full output, but flagged as
        // errors so the model treats the state change as real and adapts.
        if code == 0 {
            Ok(format!("exit code: 0\n{body}"))
        } else {
            Err(ToolError::new(format!("exit code: {code}\n{body}")))
        }
    }
}

pub(crate) fn build_command(cmd: &str, dir: &std::path::Path) -> Command {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    no_console_window(&mut c);
    c.current_dir(dir).kill_on_drop(true);
    c
}

/// CREATE_NO_WINDOW avoids console flashes for spawned processes.
/// (Platform-gated: the attribute exists only on Windows.)
#[cfg(windows)]
fn no_console_window(c: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = c.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_c: &mut Command) {}

fn truncate_lossy(bytes: &[u8], cap: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.chars().count() <= cap {
        return s.into_owned();
    }
    format!(
        "{}...[truncated, {} chars total]",
        s.chars().take(cap).collect::<String>(),
        s.chars().count()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_prefix_requirements() {
        let allow = vec!["cargo test".to_string(), "git status".to_string()];
        assert!(is_auto_approved("cargo test --lib", &allow));
        assert!(is_auto_approved("git status", &allow));
        assert!(!is_auto_approved("cargo build", &allow));
        // separators must never be auto-approved
        assert!(!is_auto_approved("cargo test; rm -rf", &allow));
        assert!(!is_auto_approved("cargo test && cargo build", &allow));
        assert!(!is_auto_approved("type a | findstr b", &allow));
        assert!(!is_auto_approved("", &allow));
    }

    #[tokio::test]
    async fn runs_portable_echo() {
        let ctx = super::super::tests::test_ctx();
        let out = RunCommand
            .run(&json!({"command": "echo kazi-ok"}), &ctx)
            .await
            .expect("echo must work on both platforms");
        assert!(out.contains("exit code: 0"));
        assert!(out.contains("kazi-ok"));
    }

    #[tokio::test]
    async fn read_only_blocks_commands() {
        let ctx = super::super::tests::test_ctx();
        ctx.permission.set(crate::tools::PermissionMode::ReadOnly);
        let err = RunCommand
            .run(&json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.0.contains("read-only"), "got: {}", err.0);
    }

    #[tokio::test]
    async fn nonzero_exit_is_error_outcome() {
        // `cmd /C "exit 3"` on Windows, `sh -c "exit 3"` elsewhere → the
        // portable trick is a command enabled in both shells: `cmd` cleanup.
        let ctx = super::super::tests::test_ctx();
        let cmd = if cfg!(windows) {
            "cmd /C exit 3"
        } else {
            "exit 3"
        };
        let err = RunCommand
            .run(&json!({"command": cmd}), &ctx)
            .await
            .unwrap_err();
        assert!(err.0.contains("exit code: 3"));
    }
}
