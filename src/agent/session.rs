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
    let mut out: Vec<(u64, String)> = vec![];
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
                        out.push((mtime, stem.to_string()));
                    }
                }
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.0));
    out.into_iter().map(|(_, n)| n).collect()
}

/// Name identifier: simple + sorted by age naturally.
pub fn default_name() -> String {
    format!("chat-{:x}", now_secs())
}

fn now_secs() -> u64 {
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
}
