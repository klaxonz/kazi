//! Local credential storage: one JSON file, provider nickname → API key.
//!
//! Keys saved via `kazi auth` live under the platform data dir
//! (`dirs::data_dir()/kazi/auth.json`, e.g. `%LOCALAPPDATA%\kazi\auth.json`
//! on Windows). Environment variables always win over stored keys, so CI
//! and containers keep working unchanged. The file is user-private by OS
//! defaults; this is deliberately NOT the config.toml (which must stay
//! shareable and key-free).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const AUTH_FILE_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthFile {
    pub version: u32,
    /// provider nickname → API key
    pub keys: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("data directory unavailable for credential storage")]
    NoDataDir,
    #[error("credential io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("credential file {0:?} is malformed: {1}")]
    Malformed(PathBuf, String),
}

pub fn auth_path() -> Result<PathBuf, AuthError> {
    dirs::data_dir()
        .map(|d| d.join("kazi").join("auth.json"))
        .ok_or(AuthError::NoDataDir)
}

/// Read the whole store; missing file = empty store (never an error for callers).
pub fn load() -> Result<AuthFile, AuthError> {
    let path = auth_path()?;
    if !path.exists() {
        return Ok(AuthFile {
            version: AUTH_FILE_VERSION,
            keys: Default::default(),
        });
    }
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice(&bytes).map_err(|e| AuthError::Malformed(path, e.to_string()))
}

/// Save (or overwrite) the key for one provider nickname.
pub fn store(nick: &str, key: &str) -> Result<PathBuf, AuthError> {
    let mut file = load()?;
    file.keys.insert(nick.to_string(), key.trim().to_string());
    write(&file)
}

/// Remove the stored key for one provider nickname. `Ok(false)` = wasn't stored.
pub fn remove(nick: &str) -> Result<bool, AuthError> {
    let mut file = load()?;
    if file.keys.remove(nick).is_some() {
        write(&file)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn write(file: &AuthFile) -> Result<PathBuf, AuthError> {
    let path = auth_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(file)
        .map_err(|e| AuthError::Malformed(path.clone(), e.to_string()))?;
    std::fs::write(&path, json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> AuthFile {
        AuthFile {
            version: AUTH_FILE_VERSION,
            keys: BTreeMap::from([("deepseek".into(), "sk-test".into())]),
        }
    }

    #[test]
    fn roundtrip_serde() {
        let a = test_store();
        let json = serde_json::to_vec_pretty(&a).unwrap();
        let b: AuthFile = serde_json::from_slice(&json).unwrap();
        assert_eq!(b.keys.get("deepseek").map(String::as_str), Some("sk-test"));
        assert_eq!(b.version, AUTH_FILE_VERSION);
    }

    #[test]
    fn remove_missing_is_false() {
        let mut a = test_store();
        assert!(a.keys.remove("nope").is_none());
        assert!(a.keys.remove("deepseek").is_some());
    }
}
