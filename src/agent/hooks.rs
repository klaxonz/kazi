//! Hooks: project-defined commands around tool calls and tasks.
//!
//! Configured in `config.toml`:
//!
//! ```toml
//! [[hooks]]
//! event = "pre_tool"        # pre_tool | post_tool | post_task
//! command = ".kazi/hooks/guard.sh"
//! timeout_secs = 10
//! on_failure = "deny"       # pre_tool only: non-zero exit refuses the call
//! ```
//!
//! The hook gets one JSON object on stdin (`event`, `tool`, `args`, `cwd`,
//! `permission`, plus `is_error`/`preview` after a tool, `ok`/`answer` after a
//! task). Exit 0 = allow; non-zero on `pre_tool` = deny, and the hook's
//! stderr/stdout becomes the reason the model sees. `post_tool` and
//! `post_task` are advisory: their output is reported but never undoes work.
//!
//! A hook that times out is treated as a failure (deny for `pre_tool`).

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub event: String,
    pub command: String,
    pub timeout: Duration,
    /// `pre_tool` only: a non-zero exit refuses the call (otherwise it warns).
    pub deny_on_failure: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Hooks {
    pub list: Vec<Hook>,
}

/// One hook's outcome as the UI/model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRun {
    pub event: String,
    pub command: String,
    pub ok: bool,
    pub output: String,
}

impl Hooks {
    pub fn from_cfg(cfg: &crate::config::Config) -> Self {
        let mut list = Vec::new();
        for h in &cfg.hooks {
            let event = h.event.trim().to_ascii_lowercase();
            if !matches!(event.as_str(), "pre_tool" | "post_tool" | "post_task") {
                if !event.is_empty() {
                    eprintln!(
                        "[hooks] event {event:?} is not one of pre_tool / post_tool / post_task — skipped"
                    );
                }
                continue;
            }
            if h.command.trim().is_empty() {
                eprintln!("[hooks] {event} entry has no command — skipped");
                continue;
            }
            let deny = match h.on_failure.as_deref() {
                Some("deny") => true,
                Some("warn") => false,
                _ => event == "pre_tool",
            };
            list.push(Hook {
                event,
                command: h.command.trim().to_string(),
                timeout: Duration::from_secs(h.timeout_secs.unwrap_or(10).clamp(1, 600)),
                deny_on_failure: deny,
            });
        }
        Self { list }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// `Some(reason)` refuses the tool call.
    pub async fn pre_tool(&self, cwd: &Path, tool: &str, args: &Value) -> Option<String> {
        let mut refusal = None;
        for hook in self.list.iter().filter(|h| h.event == "pre_tool") {
            let run = self
                .fire(
                    hook,
                    cwd,
                    json!({"event": "pre_tool", "tool": tool, "args": args}),
                )
                .await;
            if !run.ok && hook.deny_on_failure && refusal.is_none() {
                refusal = Some(format!(
                    "refused by the project's pre_tool hook ({}): {}",
                    hook.command,
                    if run.output.trim().is_empty() {
                        "no reason given"
                    } else {
                        run.output.trim()
                    }
                ));
            }
        }
        refusal
    }

    /// Advisory hooks after a tool call / a task. Never fail the run.
    pub async fn after(&self, event: &str, cwd: &Path, payload: Value) -> Vec<HookRun> {
        let mut out = Vec::new();
        for hook in self.list.iter().filter(|h| h.event == event) {
            let mut body = payload.clone();
            body["event"] = json!(event);
            out.push(self.fire(hook, cwd, body).await);
        }
        out
    }

    async fn fire(&self, hook: &Hook, cwd: &Path, payload: Value) -> HookRun {
        let mut cmd = crate::tools::shell::build_command(&hook.command, cwd);
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::tools::shell::no_console_window(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return HookRun {
                    event: hook.event.clone(),
                    command: hook.command.clone(),
                    ok: false,
                    output: format!("cannot start: {e}"),
                }
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let line = payload.to_string();
            tokio::spawn(async move {
                let _ = stdin.write_all(line.as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
            });
        }
        let waited = tokio::time::timeout(hook.timeout, child.wait_with_output()).await;
        let (ok, output) = match waited {
            Err(_) => (
                false,
                format!("timed out after {}s", hook.timeout.as_secs()),
            ),
            Ok(Err(e)) => (false, format!("wait failed: {e}")),
            Ok(Ok(out)) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                let err = String::from_utf8_lossy(&out.stderr);
                if !err.trim().is_empty() {
                    if !text.trim().is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&err);
                }
                (
                    out.status.success(),
                    crate::tools::clip_line(text.trim(), 600).to_string(),
                )
            }
        };
        HookRun {
            event: hook.event.clone(),
            command: hook.command.clone(),
            ok,
            output,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(event: &str, command: &str) -> crate::config::Config {
        crate::config::Config {
            hooks: vec![crate::config::HookCfg {
                event: event.to_string(),
                command: command.to_string(),
                timeout_secs: Some(20),
                on_failure: None,
            }],
            ..Default::default()
        }
    }

    fn cwd() -> &'static Path {
        Path::new(".")
    }

    #[tokio::test]
    async fn a_failing_pre_tool_hook_refuses_the_call() {
        let hooks = Hooks::from_cfg(&cfg_with("pre_tool", "exit 3"));
        let reason = hooks
            .pre_tool(cwd(), "run_command", &json!({"command": "x"}))
            .await
            .expect("non-zero exit must deny");
        assert!(reason.contains("pre_tool hook"), "{reason}");
        assert!(reason.contains("exit 3"), "{reason}");
    }

    #[tokio::test]
    async fn a_succeeding_pre_tool_hook_allows_it() {
        let hooks = Hooks::from_cfg(&cfg_with("pre_tool", "echo fine"));
        assert_eq!(hooks.pre_tool(cwd(), "read_file", &json!({})).await, None);
    }

    #[tokio::test]
    async fn warn_mode_never_denies_and_advisory_hooks_report() {
        let mut cfg = cfg_with("pre_tool", "exit 1");
        cfg.hooks[0].on_failure = Some("warn".to_string());
        let hooks = Hooks::from_cfg(&cfg);
        assert_eq!(hooks.pre_tool(cwd(), "read_file", &json!({})).await, None);

        let hooks = Hooks::from_cfg(&cfg_with("post_task", "echo done"));
        let runs = hooks
            .after("post_task", cwd(), json!({"ok": true, "answer": "hi"}))
            .await;
        assert_eq!(runs.len(), 1);
        assert!(runs[0].ok);
        assert_eq!(runs[0].output, "done");
        // no post_tool hooks configured -> nothing runs
        assert!(hooks.after("post_tool", cwd(), json!({})).await.is_empty());
    }

    #[test]
    fn unknown_events_and_empty_commands_are_skipped() {
        let cfg = crate::config::Config {
            hooks: vec![
                crate::config::HookCfg {
                    event: "on_lunch".into(),
                    command: "true".into(),
                    timeout_secs: None,
                    on_failure: None,
                },
                crate::config::HookCfg {
                    event: "pre_tool".into(),
                    command: "   ".into(),
                    timeout_secs: None,
                    on_failure: None,
                },
            ],
            ..Default::default()
        };
        let hooks = Hooks::from_cfg(&cfg);
        assert!(hooks.is_empty(), "{:?}", hooks.list);
        assert!(Hooks::from_cfg(&crate::config::Config::default()).is_empty());
    }
}
