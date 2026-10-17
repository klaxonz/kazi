//! `todo_write` — the model keeps a visible plan checklist.
//!
//! The whole list is replaced on every call (the model owns the structure;
//! partial updates invite desync). The engine snapshots the list after each
//! tool round and emits [`super::agent::AgentEvent::TodosUpdated`] so any UI
//! (the TUI checklist pane and the streaming `TodosUpdated` events) can
//! render it.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Tool, ToolCtx, ToolError};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    /// `pending` | `in_progress` | `completed`
    pub status: String,
}

impl TodoItem {
    fn marker(&self) -> &'static str {
        match self.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[>]",
            _ => "[ ]",
        }
    }
}

/// Shared, UI-visible plan state.
#[derive(Default)]
pub struct TodoList {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoList {
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, items: Vec<TodoItem>) {
        *self.items.lock().unwrap_or_else(|e| e.into_inner()) = items;
    }
}

fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(plan cleared)".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {} {}", i + 1, t.marker(), t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse and validate the `todos` argument.
pub fn parse_todos(args: &Value) -> Result<Vec<TodoItem>, ToolError> {
    let arr = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::new("missing `todos` array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let content = v
            .get("content")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| {
                ToolError::new(format!("todos[{i}].content must be a non-empty string"))
            })?;
        let status = match v.get("status").and_then(Value::as_str).unwrap_or("pending") {
            s @ ("pending" | "in_progress" | "completed") => s.to_string(),
            other => {
                return Err(ToolError::new(format!(
                    "todos[{i}].status `{other}` is invalid (pending | in_progress | completed)"
                )));
            }
        };
        out.push(TodoItem {
            content: content.to_string(),
            status,
        });
    }
    Ok(out)
}

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }

    fn description(&self) -> &str {
        "Record or update the task plan as a checklist. Replaces the whole list every call. Use it for multi-step work: keep exactly one item `in_progress`, mark items `completed` as they finish (do not batch completions). Keep items short and concrete."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full list, in order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Short, concrete step"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let items = parse_todos(args)?;
        if let Some(list) = &ctx.todos {
            list.replace(items.clone());
        }
        Ok(render(&items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx_with(list: Arc<TodoList>) -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: std::time::Duration::from_secs(5),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(super::super::AutoApprover),
            checkpointing: None,
            todos: Some(list),
            permission: super::super::Permission::default(),
            subagent: None,
            staged: None,
            reviewer: None,
            jobs: None,
        }
    }

    #[tokio::test]
    async fn replaces_the_whole_list_and_renders() {
        let list = Arc::new(TodoList::default());
        let ctx = ctx_with(list.clone());
        let out = TodoWrite
            .run(
                &json!({"todos": [
                    {"content": "read code", "status": "completed"},
                    {"content": "write fix", "status": "in_progress"},
                    {"content": "run tests", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("1. [x] read code"));
        assert!(out.contains("2. [>] write fix"));
        assert!(out.contains("3. [ ] run tests"));
        assert_eq!(list.snapshot().len(), 3);

        // a second call replaces, not appends
        TodoWrite
            .run(
                &json!({"todos": [{"content": "only one", "status": "pending"}]}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(list.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn invalid_entries_are_errors_not_silent_drops() {
        let list = Arc::new(TodoList::default());
        let ctx = ctx_with(list.clone());
        let err = TodoWrite
            .run(
                &json!({"todos": [{"content": "x", "status": "done"}]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("invalid"));
        assert!(list.snapshot().is_empty(), "nothing stored on error");

        let err2 = TodoWrite
            .run(&json!({"todos": [{"status": "pending"}]}), &ctx)
            .await
            .unwrap_err();
        assert!(err2.0.contains("content"));
    }
}
