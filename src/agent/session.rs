//! Session persistence: conversations survive process restarts.
//!
//! Files live under the platform data dir (`dirs::data_dir()/kazi/sessions`,
//! e.g. `%LOCALAPPDATA%\kazi\sessions` on Windows, `~/.local/share/kazi/
//! sessions` on Linux) as one JSON per session. Saving is best-effort from
//! the CLI; the library functions return `Result` for embedders.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::llm::Message;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionFile {
    pub version: u32,
    pub name: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    /// Unix seconds at save time.
    pub saved_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("data directory unavailable for session storage")]
    NoDataDir,
    #[error("session io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session file {0:?} is malformed: {1}")]
    Malformed(PathBuf, String),
}

pub fn sessions_dir() -> Result<PathBuf, SessionError> {
    dirs::data_dir()
        .map(|d| d.join("kazi").join("sessions"))
        .ok_or(SessionError::NoDataDir)
}

pub fn save(
    name: &str,
    system: Option<&str>,
    messages: &[Message],
) -> Result<PathBuf, SessionError> {
    let dir = sessions_dir()?;
    std::fs::create_dir_all(&dir)?;
    let file = SessionFile {
        version: 1,
        name: name.to_string(),
        system: system.map(str::to_string),
        messages: messages.to_vec(),
        saved_at: now_secs(),
    };
    let path = dir.join(format!("{name}.json"));
    let json = serde_json::to_vec_pretty(&file)
        .map_err(|e| SessionError::Malformed(path.clone(), e.to_string()))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

pub fn load(name: &str) -> Result<SessionFile, SessionError> {
    let dir = sessions_dir()?;
    let path = dir.join(format!("{name}.json"));
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| SessionError::Malformed(path, e.to_string()))
}

/// Names of all stored sessions, sorted newest first.
pub fn list() -> Vec<String> {
    list_summaries().into_iter().map(|s| s.name).collect()
}

/// One row of `kazi sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub name: String,
    pub messages: usize,
    pub saved_at: u64,
}

/// Stored sessions newest-first, with message counts and save times.
pub fn list_summaries() -> Vec<SessionSummary> {
    let mut out: Vec<(u64, usize, String)> = vec![];
    if let Ok(dir) = sessions_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().into_string().ok();
                let mtime = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(n) = name {
                    if let Some(stem) = n.strip_suffix(".json") {
                        let messages = std::fs::File::open(e.path())
                            .ok()
                            .and_then(|f| serde_json::from_reader::<_, SessionFile>(f).ok())
                            .map(|f| f.messages.len())
                            .unwrap_or(0);
                        out.push((mtime, messages, stem.to_string()));
                    }
                }
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    out.into_iter()
        .map(|(mtime, messages, name)| SessionSummary {
            name,
            messages,
            saved_at: mtime,
        })
        .collect()
}

/// Resolve a (possibly partial) session name: exact wins, then a unique
/// prefix among stored sessions. `None` = not found / ambiguous.
pub fn resolve(name: &str) -> Option<String> {
    if sessions_dir()
        .map(|d| d.join(format!("{name}.json")).exists())
        .unwrap_or(false)
    {
        return Some(name.to_string());
    }
    let matches: Vec<String> = list().into_iter().filter(|n| n.starts_with(name)).collect();
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

/// Render a saved session as readable markdown:
/// `## user` / `## assistant` (with tool calls as a bullet list) / blockquote
/// tool results, tied to the *tool name* via the assistant call above them.
pub fn export_markdown(f: &SessionFile) -> String {
    let mut md = format!("# kazi session `{}`\n\n", f.name);
    if let Some(sys) = &f.system {
        md.push_str(&format!("> system: {sys}\n\n"));
    }
    let mut tool_names: std::collections::BTreeMap<String, String> = Default::default();
    for m in &f.messages {
        for tc in &m.tool_calls {
            tool_names.insert(tc.id.clone(), tc.name.clone());
        }
    }
    for m in &f.messages {
        match m.role {
            crate::llm::Role::User => md.push_str(&format!("## user\n\n{}\n\n", m.text())),
            crate::llm::Role::Assistant => {
                md.push_str("## assistant\n\n");
                if !m.text().is_empty() {
                    md.push_str(&format!("{}\n\n", m.text()));
                }
                for tc in &m.tool_calls {
                    md.push_str(&format!("- calls `{}` {}\n", tc.name, tc.arguments));
                }
                if !m.tool_calls.is_empty() {
                    md.push('\n');
                }
            }
            crate::llm::Role::Tool => {
                let which = m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| tool_names.get(id))
                    .cloned()
                    .unwrap_or_else(|| "unknown tool".into());
                md.push_str(&format!(
                    "> tool `{}`{}: {}\n\n",
                    which,
                    if m.is_error { " (ERROR)" } else { "" },
                    first_lines(m.text(), 12)
                ));
            }
            crate::llm::Role::System => md.push_str(&format!("> system: {}\n\n", m.text())),
        }
    }
    md
}

fn first_lines(text: &str, max: usize) -> String {
    let mut out: String = text.lines().take(max).collect::<Vec<_>>().join(" | ");
    if text.lines().count() > max {
        out.push_str(" | ...");
    }
    out
}

/// Name identifier: simple + sorted by age naturally.
pub fn default_name() -> String {
    format!("chat-{:x}", now_secs())
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Never crash the CLI on transient fs problems: saving is best-effort.
pub fn save_quiet(name: &str, system: Option<&str>, messages: &[Message]) -> Option<PathBuf> {
    save(name, system, messages)
        .inspect_err(|e| eprintln!("(session save skipped: {e})"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, Role};

    #[test]
    fn save_load_roundtrip() {
        let name = format!("test-{}", std::process::id());
        let msgs = vec![
            Message::user("q"),
            Message::assistant("a"),
            Message::tool_result("t", "r", true),
        ];
        let path = save(&name, Some("sys"), &msgs).unwrap();
        assert!(path.exists());
        let back = load(&name).unwrap();
        assert_eq!(back.messages.len(), 3);
        assert_eq!(back.messages[0].role, Role::User);
        assert!(back.messages[2].is_error);
        assert_eq!(back.system.as_deref(), Some("sys"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn summaries_and_resolve_prefix() {
        let base = format!("prefix-test-{}", std::process::id());
        let a = format!("{base}-a");
        let b = format!("{base}-b");
        save(&a, None, &[]).unwrap();
        save(&b, None, &[]).unwrap();

        // exact wins; unique prefix resolves; ambiguous prefix is None
        assert_eq!(resolve(&a).as_deref(), Some(a.as_str()));
        assert!(
            !resolve(&format!("{base}-")).is_some(),
            "two matches → ambiguous"
        );
        assert_eq!(resolve(&format!("{base}-b")).as_deref(), Some(b.as_str()));

        let summaries = list_summaries();
        assert!(summaries.iter().any(|s| s.name == a));

        std::fs::remove_file(sessions_dir().unwrap().join(format!("{a}.json"))).unwrap();
        std::fs::remove_file(sessions_dir().unwrap().join(format!("{b}.json"))).unwrap();
    }

    #[test]
    fn export_markdown_shape() {
        let msgs = vec![
            Message::user("count files"),
            Message::assistant_with_calls(
                Some("checking".into()),
                vec![crate::llm::ToolCall {
                    id: "t1".into(),
                    name: "list_dir".into(),
                    arguments: serde_json::json!({"path": "."}),
                }],
            ),
            Message::tool_result("t1", "file.rs\nfile2.rs", false),
        ];
        let f = SessionFile {
            version: 1,
            name: "export-test".into(),
            system: Some("sys".into()),
            messages: msgs,
            saved_at: 0,
        };
        let md = export_markdown(&f);
        assert!(md.contains("# kazi session `export-test`"));
        assert!(md.contains("> system: sys"));
        assert!(md.contains("## user"));
        assert!(md.contains("## assistant"));
        assert!(md.contains("- calls `list_dir`"));
        assert!(md.contains("> tool `list_dir`:"));
        assert!(md.contains("file.rs | file2.rs"));
    }
}
