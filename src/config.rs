//! Configuration layer: one TOML file, many providers, actionable errors.
//!
//! Precedence for provider/model selection (highest first):
//! 1. CLI flags (`--provider`, `--model`)
//! 2. env overrides (`KAZI_PROVIDER`, `KAZI_MODEL`)
//! 3. top-level config keys (`provider = "..."`)
//! 4. the first `[providers.*]` entry in file order
//!
//! API keys never live in the file — entries name an env var
//! (`api_key_env`), and the value must be present in the environment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::llm::{ProviderKind, ProviderSettings};

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KAZI_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("kazi")
        .join("config.toml")
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(
        "no config file at {path}\n  →  create one with: kazi config --init\n  →  or point KAZI_CONFIG at a file"
    )]
    Missing { path: PathBuf },
    #[error("cannot parse config {path}:\n{msg}")]
    Parse { path: PathBuf, msg: String },
    #[error("provider `{nick}`: {msg}")]
    BadProvider { nick: String, msg: String },
    #[error("config {path} has no [providers.*] entries — see config.example.toml")]
    NoProviders { path: PathBuf },
    #[error("unknown provider `{0}` — configured are: {1}")]
    Unknown(String, String),
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct Config {
    /// Provider used when neither flags nor env override; absent = first entry.
    pub provider: Option<String>,
    pub agent: AgentCfg,
    pub tools: ToolsCfg,
    /// MCP stdio servers to bridge as tools (`[mcp.servers.<nick>]`).
    pub mcp: McpCfg,
    /// nickname → entry; BTreeMap keeps stable ordering for errors and `first`.
    pub providers: BTreeMap<String, ProviderEntry>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct McpCfg {
    /// nickname → server entry; each spawns a stdio MCP server process.
    pub servers: BTreeMap<String, McpServerEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerEntry {
    /// stdio transport: executable to spawn (with `args`/`env`)
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// streamable-HTTP transport: MCP endpoint URL
    pub url: Option<String>,
    /// env var holding the bearer token for HTTP servers (`Authorization`)
    pub api_key_env: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct AgentCfg {
    pub max_rounds: Option<u32>,
    pub max_context_tokens: Option<u64>,
    /// Concurrent tool executions within one round (default 4).
    pub max_parallel_tools: Option<usize>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct ToolsCfg {
    /// Sandboxes `list_dir`/`read_file`/`write_file` under this directory.
    pub sandbox_root: Option<String>,
    pub command_timeout_secs: Option<u64>,
    /// Max characters of tool output fed back into the model.
    pub output_limit: Option<usize>,
    /// `run_command` prefixes auto-approved (safety-checked).
    pub allow_commands: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// "openai" | "anthropic" | "ollama"
    pub r#type: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Name of the env var holding the key. Defaults:
    /// openai → OPENAI_API_KEY, anthropic → ANTHROPIC_API_KEY, ollama → none.
    pub api_key_env: Option<String>,
}

/// (config, its path) so the CLI can tell users where things came from.
pub fn load(explicit: Option<&Path>) -> Result<(Config, PathBuf), ConfigError> {
    let path = explicit.map(Path::to_path_buf).unwrap_or_else(config_path);
    if !path.exists() {
        return Err(ConfigError::Missing { path });
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ConfigError::Parse {
        path: path.clone(),
        msg: e.to_string(),
    })?;
    let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.clone(),
        msg: e.to_string(),
    })?;
    Ok((cfg, path))
}

#[derive(Debug)]
pub struct ResolvedProvider {
    /// The `[providers.<nick>]` key, for display.
    pub nick: String,
    pub settings: ProviderSettings,
}

impl Config {
    fn kind_of(nick: &str, entry: &ProviderEntry) -> Result<ProviderKind, ConfigError> {
        match entry.r#type.trim().to_lowercase().as_str() {
            "openai" | "openai_compatible" | "openai-compatible" => Ok(ProviderKind::OpenAiCompat),
            "anthropic" => Ok(ProviderKind::Anthropic),
            "ollama" => Ok(ProviderKind::Ollama),
            other => Err(ConfigError::BadProvider {
                nick: nick.to_string(),
                msg: format!("unknown `type = \"{other}\"` (allowed: openai | anthropic | ollama)"),
            }),
        }
    }

    fn default_key_env(kind: ProviderKind) -> Option<&'static str> {
        match kind {
            ProviderKind::OpenAiCompat => Some("OPENAI_API_KEY"),
            ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
            ProviderKind::Ollama => None,
        }
    }

    /// Resolve the API key for a provider entry from the environment.
    /// Deliberately separate from [`Config::resolve`]: embedders plug their own
    /// key source; the CLI uses this for early, actionable failures.
    pub fn api_key_for(&self, nick: &str) -> Result<Option<String>, ConfigError> {
        let entry = self
            .providers
            .get(nick)
            .ok_or_else(|| ConfigError::Unknown(nick.to_string(), self.nicks().join(", ")))?;
        let kind = Self::kind_of(nick, entry)?;
        if kind == ProviderKind::Ollama {
            return Ok(None);
        }
        let (env_name, configured) = match &entry.api_key_env {
            Some(env) => (env.clone(), true),
            None => (
                Self::default_key_env(kind)
                    .expect("non-ollama kinds have defaults")
                    .to_string(),
                false,
            ),
        };
        match std::env::var(&env_name) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(v)),
            _ => Err(ConfigError::BadProvider {
                nick: nick.to_string(),
                msg: format!(
                    "environment variable {env_name} is not set{} — export it, or use a provider with a key",
                    if configured { " (api_key_env in config)" } else { "" }
                ),
            }),
        }
    }

    /// Resolve the provider nick + concrete settings for a run.
    pub fn resolve(
        &self,
        provider_arg: Option<&str>,
        model_arg: Option<&str>,
    ) -> Result<ResolvedProvider, ConfigError> {
        let env_nick = std::env::var("KAZI_PROVIDER")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let nick = provider_arg
            .map(|s| s.to_string())
            .or(env_nick)
            .or_else(|| self.provider.clone())
            .or_else(|| self.providers.keys().next().cloned());

        let nick = nick.ok_or_else(|| ConfigError::NoProviders {
            path: config_path(),
        })?;
        let entry = self.providers.get(&nick).ok_or_else(|| {
            ConfigError::Unknown(
                nick.clone(),
                self.providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        })?;

        let env_model = std::env::var("KAZI_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let model = model_arg
            .map(|s| s.to_string())
            .or(env_model)
            .or_else(|| entry.model.clone())
            .ok_or_else(|| ConfigError::BadProvider {
                nick: nick.clone(),
                msg: "no model: set `model` in config, pass --model, or export KAZI_MODEL".into(),
            })?;

        let kind = Self::kind_of(&nick, entry)?;
        // NOTE: the API key is resolved separately (`api_key_for`), so config
        // selection stays testable and library users can supply their own key.
        Ok(ResolvedProvider {
            nick,
            settings: ProviderSettings {
                kind,
                model,
                base_url: entry.base_url.clone(),
                api_key: None,
            },
        })
    }

    /// All nicknames, for `/provider` and error messages.
    pub fn nicks(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

/// The example config ships inside the binary — `kazi config --init`.
pub const EXAMPLE_TOML: &str = include_str!("../config.example.toml");

/// Write the example to `path`. `Ok(true)` = written, `Ok(false)` = existed.
pub fn write_example(path: &Path) -> Result<bool, std::io::Error> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, EXAMPLE_TOML)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ProviderKind;

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    const SAMPLE: &str = r#"
provider = "b"

[agent]
max_rounds = 5

[tools]
command_timeout_secs = 10

[providers.a]
type = "openai"
model = "m1"
api_key_env = "TEST_KA"

[providers.b]
type = "anthropic"
model = "m2"
"#;

    #[test]
    fn parses_full_document() {
        let c = cfg(SAMPLE);
        assert_eq!(c.provider.as_deref(), Some("b"));
        assert_eq!(c.agent.max_rounds, Some(5));
        assert_eq!(c.providers.len(), 2);
        assert_eq!(c.providers["a"].r#type, "openai");
        assert_eq!(c.providers["a"].api_key_env.as_deref(), Some("TEST_KA"));
    }

    #[test]
    fn selection_flag_wins_over_config_default() {
        let c = cfg(SAMPLE);
        let resolved = c.resolve(Some("a"), None).unwrap();
        assert_eq!(resolved.nick, "a");
        assert_eq!(resolved.settings.kind, ProviderKind::OpenAiCompat);
        assert_eq!(resolved.settings.model, "m1");
        // config default applies when no flag
        let resolved_b = c.resolve(None, Some("override-model")).unwrap();
        assert_eq!(resolved_b.nick, "b");
        assert_eq!(resolved_b.settings.model, "override-model");
    }

    #[test]
    fn unknown_nick_error_lists_options() {
        let c = cfg(SAMPLE);
        let err = c.resolve(Some("zzz"), None).unwrap_err();
        match err {
            ConfigError::Unknown(_, list) => assert!(list.contains("a") && list.contains("b")),
            other => panic!("wrong error: {other}"),
        }
    }

    #[test]
    fn missing_model_is_actionable() {
        let c = cfg("[providers.z]\ntype = \"openai\"");
        let err = c.resolve(Some("z"), None).unwrap_err();
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn key_env_must_be_present() {
        let c = cfg("[providers.z]\ntype = \"openai\"\nmodel = \"m\"\napi_key_env = \"KAZI_NOPE\"");
        std::env::remove_var("KAZI_NOPE");
        let err = c.api_key_for("z").unwrap_err();
        assert!(err.to_string().contains("KAZI_NOPE"));
    }

    #[test]
    fn ollama_needs_no_key() {
        let c = cfg("[providers.z]\ntype = \"ollama\"\nmodel = \"qwen3:14b\"");
        let resolved = c.resolve(Some("z"), None).unwrap();
        assert_eq!(resolved.settings.kind, ProviderKind::Ollama);
        assert!(c.api_key_for("z").unwrap().is_none());
    }

    #[test]
    fn example_toml_itself_parses() {
        let c: Config = toml::from_str(EXAMPLE_TOML).unwrap();
        assert!(c.providers.len() >= 3);
    }
}
