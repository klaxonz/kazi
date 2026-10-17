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

/// One match from a cross-session search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHit {
    pub session: String,
    pub saved_at: u64,
    pub role: &'static str,
    /// 1-based line inside that message.
    pub line: usize,
    pub snippet: String,
}

/// Search every stored session's user/assistant text (tool output is
/// deliberately excluded: it is noise for "where did we discuss X"). Newest
/// sessions first; at most `per_session` hits each, `max_hits` overall.
pub fn search(query: &str, per_session: usize, max_hits: usize) -> Vec<SessionHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<SessionHit> = Vec::new();
    for summary in list_summaries() {
        if hits.len() >= max_hits {
            break;
        }
        let Ok(file) = load(&summary.name) else {
            continue;
        };
        let mut found = 0usize;
        for m in &file.messages {
            if found >= per_session || hits.len() >= max_hits {
                break;
            }
            let role = match m.role {
                crate::llm::Role::User => "you",
                crate::llm::Role::Assistant => "kazi",
                _ => continue,
            };
            for (i, line) in m.text().lines().enumerate() {
                if found >= per_session || hits.len() >= max_hits {
                    break;
                }
                if line.to_lowercase().contains(&needle) {
                    hits.push(SessionHit {
                        session: summary.name.clone(),
                        saved_at: summary.saved_at,
                        role,
                        line: i + 1,
                        snippet: snippet_around(line, &needle, 80),
                    });
                    found += 1;
                }
            }
        }
    }
    hits
}

/// A window around the first match, marked with `…` when clipped.
fn snippet_around(line: &str, needle: &str, width: usize) -> String {
    let lower = line.to_lowercase();
    let Some(pos) = lower.find(needle) else {
        return line.chars().take(width).collect();
    };
    let chars: Vec<char> = line.chars().collect();
    // byte position → char position (the needle may follow multi-byte text)
    let char_pos = line[..pos].chars().count();
    let start = char_pos.saturating_sub(width / 3);
    let end = (char_pos + needle.chars().count() + width / 2).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..end].iter());
    if end < chars.len() {
        out.push('…');
    }
    out.trim().to_string()
}

/// Names of all stored sessions, sorted newest first.
pub fn list() -> Vec<String> {
    list_summaries().into_iter().map(|s| s.name).collect()
}

/// One row of `kazi sessions`. `topic` = first user message (a quick scan hint).
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub name: String,
    pub messages: usize,
    pub saved_at: u64,
    pub topic: Option<String>,
    /// Working directory recorded in the session's system prompt.
    pub cwd: Option<String>,
}

/// The working directory recorded in a session's system prompt, if present.
/// Sessions are per-directory, so this is how a user tells two `chat-*`
/// sessions apart.
pub fn session_cwd(f: &SessionFile) -> Option<String> {
    let sys = f.system.as_deref()?;
    let line = sys.lines().find(|l| l.starts_with("Working directory:"))?;
    let dir = line.trim_start_matches("Working directory:").trim();
    let dir = dir.trim_end_matches('.');
    if dir.is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

/// Stored sessions newest-first, with message counts and save times.
pub fn list_summaries() -> Vec<SessionSummary> {
    let mut out: Vec<SessionSummary> = Vec::new();
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
                        let parsed = std::fs::File::open(e.path())
                            .ok()
                            .and_then(|f| serde_json::from_reader::<_, SessionFile>(f).ok());
                        let messages = parsed.as_ref().map(|f| f.messages.len()).unwrap_or(0);
                        let topic = parsed.as_ref().and_then(|f| {
                            f.messages
                                .iter()
                                .find(|m| m.role == crate::llm::Role::User)
                                .map(|m| m.text().chars().take(40).collect::<String>())
                        });
                        out.push(SessionSummary {
                            name: stem.to_string(),
                            messages,
                            saved_at: mtime,
                            topic,
                            cwd: parsed.as_ref().and_then(session_cwd),
                        });
                    }
                }
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.saved_at));
    out
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

/// Delete a stored session file. Resolves prefixes like [`resolve`].
/// `Ok(Some(dirty))` = deleted, `Ok(None)`: ambiguous, `Err`: not found.
pub fn delete(name: &str) -> Result<Option<String>, SessionError> {
    let dir = sessions_dir()?;
    let resolved = resolve(name).ok_or_else(|| {
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session matches `{name}` (or the prefix is ambiguous)"),
        ))
    })?;
    std::fs::remove_file(dir.join(format!("{resolved}.json")))?;
    Ok(Some(resolved))
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
    fn search_finds_text_across_sessions_and_reports_where() {
        let base = format!("find-test-{}", std::process::id());
        let a = format!("{base}-a");
        let b = format!("{base}-b");
        // a unique needle: earlier failed runs leave sessions behind, and a
        // global word like "kproxy" would count those too
        let needle = format!("kproxy{}", std::process::id());
        let msgs_a = vec![
            crate::llm::Message::user(format!("we discussed the {needle} delay problem")),
            crate::llm::Message::assistant(format!("the {needle} fix was to retry the socket")),
            crate::llm::Message::tool_result(
                "t1",
                format!("{needle} delay appears in tool output"),
                false,
            ),
        ];
        let msgs_b = vec![crate::llm::Message::assistant("no such topic here")];
        save(&a, None, &msgs_a).unwrap();
        save(&b, None, &msgs_b).unwrap();

        let hits = search(&needle, 3, 10);
        assert_eq!(hits.len(), 2, "tool output is not searched: {hits:?}");
        assert!(hits.iter().all(|h| h.session == a));
        assert!(hits
            .iter()
            .any(|h| h.role == "you" && h.snippet.contains("kproxy")));
        assert!(hits.iter().any(|h| h.role == "kazi"));

        // newest-first ordering is stable enough to assert the first session
        assert_eq!(hits[0].session, a);
        // an empty query matches nothing rather than everything
        assert!(search("   ", 3, 10).is_empty());
        // a miss is a miss
        assert!(search("definitely-not-present-xyz", 3, 10).is_empty());

        std::fs::remove_file(sessions_dir().unwrap().join(format!("{a}.json"))).unwrap();
        std::fs::remove_file(sessions_dir().unwrap().join(format!("{b}.json"))).unwrap();
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
            resolve(&format!("{base}-")).is_none(),
            "two matches → ambiguous"
        );
        assert_eq!(resolve(&format!("{base}-b")).as_deref(), Some(b.as_str()));

        let summaries = list_summaries();
        assert!(summaries.iter().any(|s| s.name == a));

        // delete: ambiguous prefix errors, then unique prefix deletes
        assert!(delete(&format!("{base}-")).is_err(), "ambiguous prefix");
        let deleted = delete(&format!("{base}-b")).unwrap().unwrap();
        assert_eq!(deleted, b);
        assert!(load(&b).is_err(), "file must be gone");
        assert!(delete(&b).is_err(), "already deleted → missing name");

        std::fs::remove_file(sessions_dir().unwrap().join(format!("{a}.json"))).unwrap();
    }

    #[test]
    fn session_cwd_reads_the_system_prompt() {
        let f = SessionFile {
            version: 1,
            name: "cwd-test".into(),
            system: Some("You are kazi.\nWorking directory: D:/Code/init/kazi.\n".into()),
            messages: vec![],
            saved_at: 0,
        };
        assert_eq!(session_cwd(&f).as_deref(), Some("D:/Code/init/kazi"));
        let none = SessionFile { system: None, ..f };
        assert_eq!(session_cwd(&none), None);
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
