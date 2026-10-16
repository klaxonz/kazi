//! `search_files` — recursive plain-text search (a simplified ripgrep) so the
//! model can answer "where is this used / defined" without shell access or
//! approval gates.
//!
//! Semantics kept deliberately small:
//! - literal substring match (no regex escaping pains for the model)
//! - optional filename glob translation (`*.rs`, `main*`)
//! - skips `.git`, `target`, `node_modules`, hidden dirs, binaries
//! - prints `path:line: text` for up to `max_results` hits (default 200)

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{arg_str, optional_str, Tool, ToolCtx, ToolError};

pub struct SearchFiles;

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_LINE_CHARS: usize = 200;
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".obsidian",
];

/// Minimal `*` glob: `*.rs`, `prefix*`, `*suffix`, `a*b*c` (in-order match).
pub fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() {
        return name.is_empty();
    }
    let mut rest = name;
    for (i, part) in pattern.split('*').enumerate() {
        if part.is_empty() {
            continue;
        }
        let matched = if i == 0 {
            let hit = rest.starts_with(part);
            if hit {
                rest = &rest[part.len()..];
            }
            hit
        } else if i == pattern.split('*').count() - 1 {
            let hit = rest.ends_with(part);
            if hit {
                rest = &rest[..rest.len() - part.len().min(rest.len())];
            }
            hit
        } else {
            match rest.find(part) {
                Some(p) => {
                    rest = &rest[p + part.len()..];
                    true
                }
                None => false,
            }
        };
        if !matched {
            return false;
        }
    }
    true
}

struct WalkState {
    hits: Vec<String>,
    budget: usize,
    pattern: String,
    ignore_case: bool,
    glob: Option<String>,
}

impl WalkState {
    fn matches_line(&self, line: &str) -> bool {
        if self.ignore_case {
            let p = self.pattern.to_lowercase();
            line.to_lowercase().contains(&p)
        } else {
            line.contains(&self.pattern)
        }
    }

    fn add_hit(&mut self, path: &Path, line_no: usize, text: &str) -> bool {
        let head: String = text.trim_end().chars().take(MAX_LINE_CHARS).collect();
        self.hits
            .push(format!("{}:{}: {head}", path.display(), line_no));
        if self.hits.len() >= self.budget {
            return false;
        }
        true
    }
}

fn walk_dir(dir: &Path, base: &Path, st: &mut WalkState) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let skip_dir = is_dir && (SKIP_DIRS.contains(&name.as_str()) || name.starts_with('.'));
        let glob_ok = st
            .glob
            .as_ref()
            .map(|g| glob_match(g, &name))
            .unwrap_or(true);
        if skip_dir {
            continue;
        }
        if is_dir {
            if !walk_dir(&path, base, st) {
                return false;
            }
        } else if glob_ok && !search_file(&path, base, st) {
            return false;
        }
        if st.hits.len() >= st.budget {
            return false;
        }
    }
    true
}

/// One file: skip binaries (NUL in first 8 KiB), match line by line.
fn search_file(path: &Path, base: &Path, st: &mut WalkState) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return true;
    };
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return true;
    }
    let display = path.strip_prefix(base).unwrap_or(path).to_path_buf();
    let text = String::from_utf8_lossy(&bytes);
    for (i, line) in text.lines().enumerate() {
        if st.matches_line(line) && !st.add_hit(&display, i + 1, line) {
            return false;
        }
    }
    true
}

#[async_trait]
impl Tool for SearchFiles {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Recursively search file contents for a literal substring (case-insensitive by default). Use this before read_file to locate definitions or usages. Skips .git/target/node_modules and hidden dirs. Optional `glob` filters filenames (e.g. `*.rs`)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Literal text to search for (NOT a regex)"},
                "path": {"type": "string", "description": "Directory or file to search (default: working directory)"},
                "glob": {"type": "string", "description": "Optional filename filter, e.g. `*.rs`"},
                "max_results": {"type": "integer", "description": "Max matched lines (default 200)"},
                "case_sensitive": {"type": "boolean", "description": "Default false"}
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let pattern = arg_str(args, "pattern")?.trim().to_string();
        if pattern.is_empty() {
            return Err(ToolError::new("argument `pattern` must not be empty"));
        }
        let rel = optional_str(args, "path").unwrap_or(".");
        let root = ctx.cwd.join(rel);
        if !root.exists() {
            return Err(ToolError::new(format!("path {root:?} does not exist")));
        }
        let glob_name = optional_str(args, "glob").map(str::to_string);
        let max_results = super::optional_usize(args, "max_results")
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, 2000);
        let ignore_case = !args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // keep the blocking fs walk off the async workers
        let root_for_task = root.clone();
        let glob_for_task = glob_name.clone();
        let pattern_for_task = pattern.clone();
        let hits = tokio::task::spawn_blocking(move || {
            let mut st = WalkState {
                hits: Vec::new(),
                budget: max_results,
                pattern: pattern_for_task,
                ignore_case,
                glob: glob_for_task,
            };
            if root_for_task.is_file() {
                let parent = root_for_task.parent().unwrap_or(&root_for_task);
                search_file(&root_for_task, parent, &mut st);
            } else {
                walk_dir(&root_for_task, &root_for_task, &mut st);
            }
            st.hits
        })
        .await
        .map_err(|e| ToolError::new(format!("search crashed: {e}")))?;

        if hits.is_empty() {
            return Ok(format!(
                "no matches for `{pattern}`{} under {}",
                glob_name
                    .as_ref()
                    .map(|g| format!(" (glob {g})"))
                    .unwrap_or_default(),
                root.display()
            ));
        }
        let mut out = hits.join("\n");
        if hits.len() >= max_results {
            out.push_str(&format!(
                "\n...[stopped at {max_results} hits; narrow the pattern or use glob]"
            ));
        }
        Ok(out)
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
        let d = std::env::temp_dir().join(format!("kazi-search-{name}-{id}"));
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
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b", "a____b"));
        assert!(glob_match("a*b*c", "a__b__c"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(!glob_match("*.rs", "x.rsx"));
        assert!(!glob_match("a*b", "ax"));
    }

    #[tokio::test]
    async fn finds_matches_with_numbers_and_skips() {
        let d = tempdir("fs");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("deep")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {\n    kazi_find_me();\n}\n").unwrap();
        std::fs::write(src.join("deep").join("lib.rs"), "// kazi_find_me here\n").unwrap();
        std::fs::write(d.join("target").join("x.rs"), "kazi_find_me();\n").unwrap(); // skipped
        std::fs::write(d.join("bin.dat"), "text\0kazi_find_me".as_bytes()).unwrap(); // binary skip
        std::fs::write(src.join("notes.txt"), "kazi_find_me in txt\n").unwrap();

        let ctx = ctx_for(d.clone());
        let out = SearchFiles
            .run(&json!({"pattern": "KAZI_FIND_ME", "path": "src"}), &ctx) // case-insensitive
            .await
            .unwrap();
        assert!(out.contains("main.rs:2"), "got: {out}");
        assert!(out.contains("lib.rs:1"));
        assert!(out.contains("notes.txt:1"));
        assert!(!out.contains("target"));
        assert!(!out.contains("bin.dat"));

        // glob filter keeps only .rs
        let filtered = SearchFiles
            .run(&json!({"pattern": "kazi_find_me", "glob": "*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(filtered.contains("main.rs"));
        assert!(!filtered.contains("notes.txt"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn budget_and_no_match_messages() {
        let d = tempdir("budget");
        let mut big = String::new();
        for i in 0..50 {
            big.push_str(&format!("needle {i}\n"));
        }
        std::fs::write(d.join("big.txt"), &big).unwrap();
        let ctx = ctx_for(d.clone());

        let capped = SearchFiles
            .run(&json!({"pattern": "needle", "max_results": 10}), &ctx)
            .await
            .unwrap();
        assert!(capped.contains("stopped at 10 hits"));

        let none = SearchFiles
            .run(&json!({"pattern": "absent-thing"}), &ctx)
            .await
            .unwrap();
        assert!(none.starts_with("no matches for"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn case_sensitive_mode() {
        let d = tempdir("case");
        std::fs::write(d.join("a.txt"), "Needle\nneedle\n").unwrap();
        let ctx = ctx_for(d.clone());
        let out = SearchFiles
            .run(&json!({"pattern": "needle", "case_sensitive": true}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("a.txt:2"));
        assert!(!out.contains("a.txt:1"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
