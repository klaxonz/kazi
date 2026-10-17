//! `apply_patch` — surgical text edits (find & replace on whole files).
//!
//! The model produces the full replacement block (not line numbers), and this
//! tool insists on *uniqueness* of the match: a fuzzy or ambiguous find is an
//! error that teaches the model to widen the context instead of guessing.
//! This is the difference between a model that edits reliably and one that
//! rewrites whole files, clobbering concurrent changes.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{arg_str, Tool, ToolCtx, ToolError};

pub struct ApplyPatch;

/// Same read budget as `read_file` edits — an apply_patch payload > 2 MiB
/// signals the model is doing a whole-file rewrite, which is `write_file`'s
/// job (and gets its own guard there).
const PATCH_CAP: usize = 2 * 1024 * 1024;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Replace the FIRST, exact, UNIQUE occurrence of `find` with `replace` inside a text file (UTF-8). Prefer patching over rewriting: the whole file is what changes otherwise. If `find` matches more than once or not at all, include more surrounding lines for context. Use empty `replace` to delete text. Read the file first."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit"},
                "find": {"type": "string", "description": "Exact text to be replaced; must occur exactly once — include 2-3 leading context lines to disambiguate"},
                "replace": {"type": "string", "description": "Replacement text (empty string deletes the `find` text)"}
            },
            "required": ["path", "find", "replace"]
        })
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = super::arg_str(args, "path")?;
        let find = arg_str(args, "find")?;
        let replace = arg_str(args, "replace")?;

        let path = ctx.cwd.join(rel);
        crate::tools::file::ensure_sandbox(ctx.sandbox_root.as_deref(), &path)?;
        if !ctx.permission.get().writes_allowed() {
            return Err(ToolError::new(
                "apply_patch is blocked in read-only permission mode (Shift+Tab / /permission to change)",
            ));
        }

        if find.is_empty() {
            return Err(ToolError::new(
                "`find` must be a non-empty, exact text block that occurs exactly once in the file",
            ));
        }
        if find.len() + replace.len() > PATCH_CAP {
            return Err(ToolError::new(format!(
                "patch too large ({} bytes); split it into smaller apply_patch calls",
                find.len() + replace.len()
            )));
        }

        // In review mode an earlier `write_file` in the same round may already
        // have parked a new version of this file: patch THAT text, not the
        // stale disk copy, or the two changes would silently disagree.
        let staging = ctx.staged.as_ref().filter(|_| ctx.reviewing());
        let staged_before = staging.and_then(|s| s.pending_text(&path));
        let text = match staged_before.clone() {
            Some(t) => t,
            None => {
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| ToolError::new(format!("cannot read {path:?}: {e}")))?;
                if bytes.iter().take(8192).any(|b| *b == 0) {
                    return Err(ToolError::new(format!(
                        "{path:?} looks binary; apply_patch is text-only"
                    )));
                }
                String::from_utf8(bytes).map_err(|_| {
                    ToolError::new(format!(
                        "{path:?} has parts that are not valid UTF-8; apply_patch is UTF-8-only"
                    ))
                })?
            }
        };

        let first = match text.find(find) {
            Some(i) => i,
            None => {
                let hint = if find.contains('\r') {
                    "note: `find` contains CR (\\r) characters — files use LF; drop them"
                } else if find.starts_with(|c: char| !c.is_whitespace()) && !find.contains('\n') {
                    "tip: a single line is easy to match twice; include surrounding lines"
                } else {
                    "tip: re-read the file and copy the block, including indentation, from the numbered output"
                };
                return Err(ToolError::new(format!(
                    "`find` text not found in {path:?}. {hint}"
                )));
            }
        };
        let second = text[first + 1..].find(find).map(|p| first + 1 + p);
        if second.is_some() {
            // report the first match's line so the model can widen context
            let line = text[..first].matches('\n').count() + 1;
            return Err(ToolError::new(format!(
                "`find` matches at least 2 times (first at line {line}) — widen the `find` block with more context so it is unique"
            )));
        }

        let mut patched = String::with_capacity(text.len() - find.len() + replace.len());
        patched.push_str(&text[..first]);
        patched.push_str(replace);
        patched.push_str(&text[first + find.len()..]);

        let start_line = text[..first].matches('\n').count() + 1;
        if let Some(staging) = staging {
            // The pre-round content: what the file held before ANY staged
            // change touched it (a follow-up patch must not report its own
            // predecessor's text as the baseline).
            let before = crate::tools::checkpoint::read_text_lossy(&path);
            staging.stage(crate::tools::checkpoint::StagedChange {
                tool: self.name().to_string(),
                path: path.clone(),
                before,
                after: patched,
            });
            let delta: i64 = replace.chars().count() as i64 - find.chars().count() as i64;
            return Ok(format!(
                "staged a patch to {rel} at line {start_line} ({delta:+} chars) for review — nothing written yet"
            ));
        }
        // write-ahead checkpoint: keep the pre-patch content for /undo
        if let Some(cp) = &ctx.checkpointing {
            cp.snapshot(&path);
        }
        tokio::fs::write(&path, patched.as_bytes())
            .await
            .map_err(|e| ToolError::new(format!("cannot write {path:?}: {e}")))?;
        let delta: i64 = replace.chars().count() as i64 - find.chars().count() as i64;
        Ok(format!(
            "patched {path:?} at line {start_line}; replaced {} bytes with {} bytes ({delta:+} chars)",
            find.len(),
            replace.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn tempdir(name: &str) -> PathBuf {
        let id = std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
        let d = std::env::temp_dir().join(format!("kazi-patch-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx_for(cwd: PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            sandbox_root: None,
            timeout: std::time::Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(super::super::AutoApprover),
            checkpointing: None,
            todos: None,
            permission: super::super::Permission::default(),
            subagent: None,
            staged: None,
            reviewer: None,
        }
    }

    async fn setup() -> (PathBuf, ToolCtx) {
        let d = tempdir("fs");
        tokio::fs::write(
            d.join("code.rs"),
            "fn main() {\n    a();\n    b();\n    c();\n}\n",
        )
        .await
        .unwrap();
        let ctx = ctx_for(d.clone());
        (d, ctx)
    }

    #[tokio::test]
    async fn replaces_unique_block() {
        let (d, ctx) = setup().await;
        let out = ApplyPatch
            .run(
                &json!({"path": "code.rs", "find": "    b();\n    c();", "replace": "    d();"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("patched"));
        let text = std::fs::read_to_string(d.join("code.rs")).unwrap();
        assert_eq!(text, "fn main() {\n    a();\n    d();\n}\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn deletes_when_replace_empty() {
        let (d, ctx) = setup().await;
        ApplyPatch
            .run(
                &json!({"path": "code.rs", "find": "    b();\n", "replace": ""}),
                &ctx,
            )
            .await
            .unwrap();
        let text = std::fs::read_to_string(d.join("code.rs")).unwrap();
        assert_eq!(text, "fn main() {\n    a();\n    c();\n}\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn ambiguous_match_is_rejected() {
        let (d, ctx) = setup().await;
        // `();` occurs on all three call lines — must be refused
        let err = ApplyPatch
            .run(
                &json!({"path": "code.rs", "find": "();", "replace": "();"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("matches at least 2 times"), "got: {}", err.0);
        assert!(err.0.contains("widen"));
        // nothing was written back
        let text = std::fs::read_to_string(d.join("code.rs")).unwrap();
        assert_eq!(text, "fn main() {\n    a();\n    b();\n    c();\n}\n");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn missing_match_teaches_retry() {
        let (d, ctx) = setup().await;
        let err = ApplyPatch
            .run(
                &json!({"path": "code.rs", "find": "    zzz();", "replace": "    x();"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("not found"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn cr_hint_for_windows_edits() {
        let (d, ctx) = setup().await;
        let err = ApplyPatch
            .run(
                &json!({"path": "code.rs", "find": "    b();\r\n", "replace": "    x();\r\n"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("CR"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn insert_via_adjacent_anchor() {
        // insert BEFORE "fn main" by anchoring on it and prepending
        let (d, ctx) = setup().await;
        ApplyPatch
            .run(&json!({"path": "code.rs", "find": "fn main()", "replace": "use core::hint::black_box;\n\nfn main()"}), &ctx)
            .await
            .unwrap();
        let text = std::fs::read_to_string(d.join("code.rs")).unwrap();
        assert!(text.starts_with("use core::hint::black_box;\n\nfn main()"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
