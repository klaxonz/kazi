//! Filesystem tools: `list_dir`, `read_file`, `write_file`.
//!
//! All paths go through [`ensure_sandbox`]; when a sandbox root is configured
//! in `config.toml` (`tools.sandbox_root`), nothing outside it is touchable —
//! resolved symlinks included, since we canonicalize before comparing.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{Tool, ToolCtx, ToolError};

/// Ensure a path is usable under the optional sandbox root.
///
/// `write` targets may not exist yet, so we canonicalize the nearest existing
/// ancestor and re-check the whole path against the canonicalized root.
pub(crate) fn ensure_sandbox(root: Option<&Path>, target: &Path) -> Result<(), ToolError> {
    let Some(root) = root else { return Ok(()) };

    let probe = if target.exists() {
        target.to_path_buf()
    } else {
        let mut cur = target.to_path_buf();
        while !cur.as_path().exists() {
            match cur.parent() {
                Some(p) if !p.as_os_str().is_empty() => cur = p.to_path_buf(),
                _ => return Ok(()), // nothing exists yet; nothing to leak
            }
        }
        cur
    };

    let root_cx = root
        .canonicalize()
        .map_err(|e| ToolError::new(format!("sandbox root invalid: {e}")))?;
    let probe_cx = probe
        .canonicalize()
        .map_err(|e| ToolError::new(format!("cannot resolve {target:?}: {e}")))?;
    if !probe_cx.starts_with(&root_cx) {
        return Err(ToolError::new(format!(
            "path {:?} is outside the sandbox root {:?} (tools.sandbox_root in config)",
            target, root
        )));
    }
    Ok(())
}

/// ---------------------------------------------------------------------------
/// list_dir
/// ---------------------------------------------------------------------------
pub struct ListDir;

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the entries of a directory (name, dirs with `/`, file sizes in bytes). Survey a location before reading files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Directory to list; defaults to the working directory"}},
            "required": []
        })
    }
    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = super::optional_str(args, "path").unwrap_or(".");
        let dir = ctx.cwd.join(rel);
        ensure_sandbox(ctx.sandbox_root.as_deref(), &dir)?;
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| ToolError::new(format!("cannot list {dir:?}: {e}")))?;
        let mut items: Vec<(String, bool, Option<u64>)> = Vec::new();
        while let Some(e) = entries.next_entry().await? {
            let meta = e.metadata().await?;
            let is_dir = meta.is_dir();
            let size = if is_dir { None } else { Some(meta.len()) };
            items.push((e.file_name().to_string_lossy().into_owned(), is_dir, size));
        }
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if items.is_empty() {
            return Ok(format!("(empty directory: {})", dir.display()));
        }
        let mut out = String::new();
        let total = items.len();
        for (name, is_dir, size) in items.into_iter().take(2000) {
            if is_dir {
                out.push_str(&format!("{name}/\n"));
            } else {
                let shown = size
                    .map(|s| format!("{s}B"))
                    .unwrap_or_else(|| "?".to_string());
                out.push_str(&format!("{name} ({shown})\n"));
            }
        }
        if total > 2000 {
            out.push_str(&format!("...[truncated, {} entries total]\n", total));
        }
        Ok(out)
    }
}

/// ---------------------------------------------------------------------------
/// read_file
/// ---------------------------------------------------------------------------
pub struct ReadFile;

const READ_CAP: usize = 256 * 1024; // do not blow up the context window
const MAX_LINES_PER_READ: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a text file with 1-based line numbers. Binary files are refused. `offset` (1-based start line) and `limit` (max lines) page through large files."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "offset": {"type": "integer", "description": "1-based start line (default 1)"},
                "limit": {"type": "integer", "description": "Max lines to read (default 2000, max 2000)"}
            },
            "required": ["path"]
        })
    }
    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = arg_path(args)?;
        let path = ctx.cwd.join(rel);
        ensure_sandbox(ctx.sandbox_root.as_deref(), &path)?;
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::new(format!("cannot read {path:?}: {e}")))?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Err(ToolError::new(format!(
                "{path:?} looks binary (NUL byte in first 8 KiB); use run_command for binary inspection"
            )));
        }
        let total_bytes = bytes.len();
        let text = String::from_utf8_lossy(&bytes[..total_bytes.min(READ_CAP)]);
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();

        let offset = super::optional_usize(args, "offset").unwrap_or(1).max(1);
        if offset > total_lines.max(1) {
            return Err(ToolError::new(format!(
                "offset {offset} is beyond the end ({total_lines} lines); use a smaller offset"
            )));
        }
        let limit = super::optional_usize(args, "limit")
            .unwrap_or(MAX_LINES_PER_READ)
            .clamp(1, MAX_LINES_PER_READ);
        let end = (offset - 1 + limit).min(total_lines);

        let mut out = String::new();
        for (n, line) in lines[offset - 1..end].iter().enumerate() {
            let n = offset + n;
            let mut l = *line;
            if l.ends_with('\r') {
                l = &l[..l.len() - 1];
            }
            let shown: String = if l.chars().count() > MAX_LINE_CHARS {
                l.chars().take(MAX_LINE_CHARS).collect::<String>() + "...[line truncated]"
            } else {
                l.to_string()
            };
            out.push_str(&format!("{n}: {shown}\n"));
        }
        if end < total_lines {
            out.push_str(&format!(
                "...[showing lines {offset}..{end} of {total_lines}; repeat read_file with offset={} limit={} to continue]\n",
                end + 1,
                MAX_LINES_PER_READ.min(total_lines - end)
            ));
        }
        if total_bytes > READ_CAP {
            out.push_str(&format!(
                "...[file is {total_bytes} bytes; only the first {READ_CAP} were read]\n"
            ));
        }
        if out.trim().is_empty() {
            out.push_str("(empty file)\n");
        }
        Ok(out)
    }
}

/// ---------------------------------------------------------------------------
/// write_file
/// ---------------------------------------------------------------------------
pub struct WriteFile;

const WRITE_CAP: usize = 2 * 1024 * 1024;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Create or (completely) overwrite a text file (UTF-8). Parent directories are created. Read a file before overwriting it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path"},
                "content": {"type": "string", "description": "Full text file content"}
            },
            "required": ["path", "content"]
        })
    }
    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = arg_path(args)?;
        let content = super::arg_str(args, "content")?;
        let path = ctx.cwd.join(rel);
        ensure_sandbox(ctx.sandbox_root.as_deref(), &path)?;
        if ctx.permission.get() == crate::tools::PermissionMode::ReadOnly {
            return Err(ToolError::new(
                "write_file is blocked in read-only permission mode (Shift+Tab / /permission to change)",
            ));
        }
        if content.len() > WRITE_CAP {
            return Err(ToolError::new(format!(
                "content is {} bytes, over the {WRITE_CAP}-byte write limit",
                content.len()
            )));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::new(format!("cannot create parent dirs: {e}")))?;
        }
        // write-ahead checkpoint: keep the pre-write content for /undo
        if let Some(cp) = &ctx.checkpointing {
            cp.snapshot(&path);
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| ToolError::new(format!("cannot write {path:?}: {e}")))?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

/// Path-like argument that must be a string; the error text teaches models the
/// correct shape so they self-correct on the next round.
fn arg_path(args: &Value) -> Result<&str, ToolError> {
    super::arg_str(args, "path")
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
        let d = std::env::temp_dir().join(format!("kazi-test-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx_for(cwd: PathBuf, sandbox: Option<PathBuf>) -> ToolCtx {
        ToolCtx {
            cwd,
            sandbox_root: sandbox,
            timeout: std::time::Duration::from_secs(10),
            output_limit: 4096,
            allow_commands: vec![],
            approver: Arc::new(super::super::AutoApprover),
            checkpointing: None,
            todos: None,
            permission: super::super::Permission::default(),
            subagent: None,
        }
    }

    #[tokio::test]
    async fn write_file_snapshots_for_undo() {
        let d = tempdir("ckpt");
        let store = std::sync::Arc::new(crate::tools::checkpoint::Checkpointing::new(
            d.join("store"),
        ));
        let mut ctx = ctx_for(d.clone(), None);
        ctx.checkpointing = Some(store.clone());

        std::fs::write(d.join("a.txt"), "v1").unwrap();
        store.begin_epoch();
        WriteFile
            .run(&json!({"path": "a.txt", "content": "v2"}), &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "v2");

        let crate::tools::checkpoint::UndoOutcome::Replayed(done) = store.undo_latest().unwrap()
        else {
            panic!("epoch exists");
        };
        assert_eq!(done.restored, vec![d.join("a.txt")]);
        assert_eq!(
            std::fs::read_to_string(d.join("a.txt")).unwrap(),
            "v1",
            "undo must restore the pre-write content"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn read_only_blocks_writes() {
        let d = tempdir("ro");
        let ctx = ctx_for(d.clone(), None);
        ctx.permission.set(super::super::PermissionMode::ReadOnly);
        let err = WriteFile
            .run(&json!({"path": "x.txt", "content": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.0.contains("read-only"), "got: {}", err.0);
        assert!(!d.join("x.txt").exists(), "nothing may be written");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let d = tempdir("fs");
        let ctx = ctx_for(d.clone(), None);
        let wrote = WriteFile
            .run(&json!({"path": "sub/a.txt", "content": "x\ny"}), &ctx)
            .await
            .unwrap();
        assert!(wrote.contains("wrote 3 bytes"));
        let read = ReadFile
            .run(&json!({"path": "sub/a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(read.contains("1: x"));
        assert!(read.contains("2: y"));
        let list = ListDir.run(&json!({}), &ctx).await.unwrap();
        assert!(list.contains("sub/"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn sandbox_blocks_outside_paths() {
        let outer = tempdir("outer");
        let inside = outer.join("work");
        std::fs::create_dir_all(&inside).unwrap();
        let ctx = ctx_for(inside.clone(), Some(inside.clone()));
        let err = ReadFile
            .run(&json!({"path": "../secret.txt"}), &ctx)
            .await
            .unwrap_err();
        assert!(err.0.contains("outside the sandbox"));
        let _ = std::fs::remove_dir_all(&outer);
    }

    #[tokio::test]
    async fn paging_with_offset_limit() {
        let d = tempdir("page");
        let ctx = ctx_for(d.clone(), None);
        let big: String = (1..=30).map(|i| format!("line{i}\n")).collect();
        WriteFile
            .run(&json!({"path": "big.txt", "content": big}), &ctx)
            .await
            .unwrap();
        let page = ReadFile
            .run(&json!({"path": "big.txt", "offset": 25, "limit": 3}), &ctx)
            .await
            .unwrap();
        assert!(page.contains("25: line25"));
        assert!(page.contains("27: line27"));
        assert!(page.contains("continue"));
        // reading to the end must NOT ask to continue
        let tail = ReadFile
            .run(&json!({"path": "big.txt", "offset": 28}), &ctx)
            .await
            .unwrap();
        assert!(tail.contains("30: line30"));
        assert!(!tail.contains("continue"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
