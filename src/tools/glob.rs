//! `glob` — find files by path pattern.
//!
//! Models otherwise discover files by walking `list_dir` one directory at a
//! time, burning rounds. A single `glob "**/*.rs"` answers "where does this
//! live" in one call. Patterns are the familiar `*`, `?` and `**` forms;
//! `.git`/`target`/`node_modules`-style directories are skipped like every
//! other kazi file tool.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{arg_str, optional_str, Tool, ToolCtx, ToolError};

pub struct Glob;

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_DEPTH: usize = 30;
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
    ".idea",
    ".vscode",
];

/// Match one path segment against a pattern segment (`*`, `?`).
pub fn segment_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // classic backtracking matcher; patterns are tiny so O(n*m) is fine
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Match a full relative path (forward slashes) against a glob pattern.
/// A pattern without `/` matches the file name at any depth.
pub fn path_match(pattern: &str, rel: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let rel = rel.replace('\\', "/");
    let (pattern, rel) = if cfg!(windows) {
        (pattern.to_lowercase(), rel.to_lowercase())
    } else {
        (pattern, rel)
    };
    if !pattern.contains('/') {
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        return segment_match(&pattern, name);
    }
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat, &segs)
}

fn match_segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => {
            // `**` matches zero or more whole segments
            if match_segments(rest, segs) {
                return true;
            }
            match segs.split_first() {
                Some((_, tail)) => match_segments(pat, tail),
                None => false,
            }
        }
        Some((&p, rest)) => match segs.split_first() {
            Some((s, tail)) => segment_match(p, s) && match_segments(rest, tail),
            None => false,
        },
    }
}

struct Walk {
    pattern: String,
    include_dirs: bool,
    max_results: usize,
    /// relative to the search root (for display)
    base: PathBuf,
    hits: Vec<(String, bool, u64, u64)>,
}

impl Walk {
    fn push(&mut self, path: &Path, is_dir: bool, size: u64, mtime: u64) -> bool {
        let rel = path.strip_prefix(&self.base).unwrap_or(path);
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if !path_match(&self.pattern, &rel_s) {
            return true;
        }
        self.hits.push((rel_s, is_dir, size, mtime));
        self.hits.len() < self.max_results
    }
}

fn walk_dir(dir: &Path, depth: usize, st: &mut Walk) -> bool {
    if depth > MAX_DEPTH {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    let mut items: Vec<_> = entries.flatten().collect();
    items.sort_by_key(|e| e.file_name());
    for e in items {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir();
        let meta = e.metadata().ok();
        let size = meta
            .as_ref()
            .map(|m| if is_dir { 0 } else { m.len() })
            .unwrap_or(0);
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if is_dir {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            if st.include_dirs && !st.push(&path, true, 0, mtime) {
                return false;
            }
            if !walk_dir(&path, depth + 1, st) {
                return false;
            }
        } else if !st.push(&path, false, size, mtime) {
            return false;
        }
    }
    true
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files (and optionally directories) by path pattern. Supports `*`, `?` and `**` (recursive), e.g. `**/*.rs`, `src/**/mod.rs`, `*.toml`. Much cheaper than walking with list_dir. Paths are printed relative to the working directory; hidden/build directories are skipped."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. `**/*.rs` (a pattern without `/` matches file names at any depth)"},
                "path": {"type": "string", "description": "Directory to search (default: working directory)"},
                "include_dirs": {"type": "boolean", "description": "Also return matching directories (default false)"},
                "max_results": {"type": "integer", "description": "Maximum entries to return (default 200)"},
                "newest_first": {"type": "boolean", "description": "Sort by modification time, newest first (default false: path order)"}
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
        super::file::ensure_sandbox(ctx.sandbox_root.as_deref(), &root)?;
        if !root.exists() {
            return Err(ToolError::new(format!("path {root:?} does not exist")));
        }
        let include_dirs = args
            .get("include_dirs")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let newest_first = args
            .get("newest_first")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let max_results = super::optional_usize(args, "max_results")
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, 2000);

        let mut st = Walk {
            pattern: pattern.clone(),
            include_dirs,
            max_results,
            base: root.clone(),
            hits: Vec::new(),
        };
        if root.is_file() {
            let name = root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if path_match(&pattern, &name) {
                let size = std::fs::metadata(&root).map(|m| m.len()).unwrap_or(0);
                st.hits.push((name, false, size, 0));
            }
        } else {
            walk_dir(&root, 0, &mut st);
        }

        if st.hits.is_empty() {
            return Ok(format!(
                "no files match `{pattern}` under {}",
                root.display()
            ));
        }
        if newest_first {
            st.hits
                .sort_by(|a, b| b.3.cmp(&a.3).then_with(|| a.0.cmp(&b.0)));
        } else {
            st.hits.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let count = st.hits.len();
        let mut out = String::new();
        for (path, is_dir, size, _) in st.hits {
            if is_dir {
                out.push_str(&format!("{path}/\n"));
            } else {
                out.push_str(&format!("{path} ({size}B)\n"));
            }
        }
        if count >= max_results {
            out.push_str(&format!(
                "...[stopped at {max_results} entries; narrow the pattern]"
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn segment_matching_supports_star_and_question() {
        assert!(segment_match("*.rs", "main.rs"));
        assert!(segment_match("main.?s", "main.rs"));
        assert!(segment_match("*", "anything"));
        assert!(segment_match("a*b*c", "a__b__c"));
        assert!(!segment_match("*.rs", "main.py"));
        assert!(!segment_match("a*b", "ax"));
    }

    #[test]
    fn path_matching_handles_double_star() {
        assert!(path_match("**/*.rs", "src/tools/file.rs"));
        assert!(path_match("**/*.rs", "main.rs"), "** matches zero dirs");
        assert!(path_match("src/**/mod.rs", "src/mod.rs"));
        assert!(path_match("src/**/mod.rs", "src/a/b/mod.rs"));
        assert!(!path_match("src/**/mod.rs", "tests/mod.rs"));
        assert!(path_match("*.md", "docs/README.md"), "no slash = basename");
        assert!(!path_match("*.md", "docs/README.md.bak"));
        assert!(path_match("src/*.rs", "src/main.rs"));
        assert!(!path_match("src/*.rs", "src/a/main.rs"));
    }

    fn tempdir(name: &str) -> PathBuf {
        let id = std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
        let d = std::env::temp_dir().join(format!("kazi-glob-{name}-{id}"));
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
            jobs: None,
        }
    }

    #[tokio::test]
    async fn finds_nested_files_and_skips_build_dirs() {
        let d = tempdir("walk");
        std::fs::create_dir_all(d.join("src/deep")).unwrap();
        std::fs::create_dir_all(d.join("target")).unwrap();
        std::fs::create_dir_all(d.join(".git")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("src/deep/mod.rs"), "").unwrap();
        std::fs::write(d.join("README.md"), "hi").unwrap();
        std::fs::write(d.join("target/generated.rs"), "").unwrap();
        std::fs::write(d.join(".git/hidden.rs"), "").unwrap();
        let ctx = ctx_for(d.clone());

        let out = Glob
            .run(&json!({"pattern": "**/*.rs"}), &ctx)
            .await
            .unwrap();
        assert!(out.contains("src/main.rs"), "{out}");
        assert!(out.contains("src/deep/mod.rs"));
        assert!(!out.contains("target"), "{out}");
        assert!(!out.contains(".git"));

        let md = Glob.run(&json!({"pattern": "*.md"}), &ctx).await.unwrap();
        assert!(md.contains("README.md"));
        assert!(!md.contains("main.rs"));

        // a subdirectory root reports paths relative to that root
        let sub = Glob
            .run(&json!({"pattern": "*.rs", "path": "src"}), &ctx)
            .await
            .unwrap();
        assert!(sub.contains("main.rs"), "{sub}");
        assert!(!sub.contains("README"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn result_budget_is_reported() {
        let d = tempdir("budget");
        for i in 0..30 {
            std::fs::write(d.join(format!("f{i}.txt")), "x").unwrap();
        }
        let ctx = ctx_for(d.clone());
        let out = Glob
            .run(&json!({"pattern": "*.txt", "max_results": 5}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.lines().count(), 6, "5 entries + note: {out}");
        assert!(out.contains("stopped at 5 entries"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn directories_are_opt_in() {
        let d = tempdir("dirs");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::create_dir_all(d.join("tests")).unwrap();
        let ctx = ctx_for(d.clone());
        let plain = Glob.run(&json!({"pattern": "src*"}), &ctx).await.unwrap();
        assert!(plain.starts_with("no files match"), "{plain}");
        let dirs = Glob
            .run(&json!({"pattern": "src", "include_dirs": true}), &ctx)
            .await
            .unwrap();
        assert!(dirs.contains("src/"), "{dirs}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
