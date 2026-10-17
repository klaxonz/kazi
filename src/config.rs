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

use crate::llm::{ProviderKind, ProviderSettings, Tuning};

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
    #[error("no [providers.*] entries configured\n  →  create a config with: kazi config --init")]
    NoProviders,
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
    /// Terminal appearance (`[ui] theme = "dark"`, `details = false`).
    pub ui: UiCfg,
    /// MCP stdio servers to bridge as tools (`[mcp.servers.<nick>]`).
    pub mcp: McpCfg,
    /// Token prices (`[usage.prices."<model>"]`) for `/cost`.
    pub usage: UsageCfg,
    /// nickname → entry; BTreeMap keeps stable ordering for errors and `first`.
    pub providers: BTreeMap<String, ProviderEntry>,
}

/// `[ui]`: look and default verbosity of the full-screen TUI.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct UiCfg {
    /// Colour theme: `dark` (default), `light`, `mono`.
    pub theme: Option<String>,
    /// Start with full tool output expanded (`Ctrl+O` toggles at runtime).
    pub details: Option<bool>,
    /// Ring the terminal bell when a task finishes (default false).
    pub bell: Option<bool>,
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
    /// Provider retries for rate limits / 5xx / network errors (default 3).
    pub max_retries: Option<u32>,
    /// Startup permission mode: "read-only" | "review" | "edit" (default) | "yolo".
    pub permission_mode: Option<String>,
    /// Check to run after a round that changed files (e.g.
    /// "cargo clippy --all-targets"); its output goes back to the model.
    pub feedback_command: Option<String>,
    /// Active request preset (`fast` | `balanced` (default) | `deep`), or the
    /// name of an entry in `presets`. `/effort` writes this line back.
    pub preset: Option<String>,
    /// Named request presets (`[agent.presets.deep]`).
    pub presets: BTreeMap<String, PresetCfg>,
    /// Stop the task between rounds once its accumulated cost exceeds this
    /// (USD). Requires `[usage.prices]` for the active model; without a price
    /// there is nothing to compare against, so the guard stays inert.
    pub max_usd_per_task: Option<f64>,
}

/// One model-effort preset: request-level knobs applied on top of the
/// provider entry (they never modify `[providers.*]`).
#[derive(Debug, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct PresetCfg {
    pub temperature: Option<f64>,
    /// Output budget; Anthropic requires one (kazi used a hard-coded 4096).
    pub max_tokens: Option<u32>,
    /// Reasoning effort passed through where the endpoint understands it
    /// (`"low"` | `"medium"` | `"high"`); ignored elsewhere.
    pub reasoning_effort: Option<String>,
}

/// `[usage]`: what a token costs, so `/cost` can print money instead of
/// guesses. Prices are USD per 1M tokens and come from the user — kazi never
/// ships a table that silently goes stale.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct UsageCfg {
    /// model id → price entry.
    pub prices: BTreeMap<String, PriceCfg>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct PriceCfg {
    /// USD per 1M input tokens.
    pub input: Option<f64>,
    /// USD per 1M output tokens.
    pub output: Option<f64>,
    /// USD per 1M cached input tokens (defaults to `input`).
    pub cache_read: Option<f64>,
    /// USD per 1M cache-write tokens (defaults to `input`).
    pub cache_write: Option<f64>,
}

impl PriceCfg {
    /// Cost of one usage sample in USD, or `None` when the entry has no
    /// numbers at all (an empty table entry must not print $0.000).
    pub fn cost(&self, usage: &crate::llm::Usage) -> Option<f64> {
        let (input, output, cache_read, cache_write) = usage.totals();
        if self.input.is_none() && self.output.is_none() {
            return None;
        }
        let per_million = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        let in_price = self.input.unwrap_or(0.0);
        let total = per_million(input, in_price)
            + per_million(output, self.output.unwrap_or(0.0))
            + per_million(cache_read, self.cache_read.unwrap_or(in_price))
            + per_million(cache_write, self.cache_write.unwrap_or(in_price));
        Some(total)
    }
}

/// The request knobs actually sent to a provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestTuning {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,
}

/// Built-in presets, used when config names a preset but does not define it.
pub fn builtin_presets() -> BTreeMap<&'static str, RequestTuning> {
    let tuning = |t: Option<f64>, m: Option<u32>, r: Option<&str>| RequestTuning {
        temperature: t,
        max_tokens: m,
        reasoning_effort: r.map(str::to_string),
    };
    BTreeMap::from([
        ("fast", tuning(Some(0.3), Some(2048), Some("low"))),
        ("balanced", tuning(Some(0.7), Some(4096), None)),
        ("deep", tuning(Some(0.2), Some(8192), Some("high"))),
    ])
}

/// Per-session cost ledger (`/cost`): what the provider charged, priced with
/// the user's own table. Unknown prices stay `None` and are shown as tokens.
#[derive(Debug, Clone, Default)]
pub struct CostLedger {
    pub usage_total: crate::llm::Usage,
    /// USD accumulated for turns whose model had a price entry.
    pub usd: f64,
    /// True once at least one round was actually priced.
    pub priced: bool,
    /// model id → its accumulated usage and cost.
    pub by_model: BTreeMap<String, (crate::llm::Usage, f64)>,
}

impl Config {
    /// Prices configured for a model id, if any.
    pub fn price_for(&self, model: &str) -> Option<&PriceCfg> {
        self.usage.prices.get(model).or_else(|| {
            // a gateway id like `commandcode/deepseek/deepseek-v4-pro` should
            // match a price keyed on its last segment too
            model
                .rsplit('/')
                .next()
                .and_then(|tail| self.usage.prices.get(tail))
        })
    }

    /// The active request preset: `[agent.presets.*]` first, then the
    /// built-ins. Unknown names fall back to balanced.
    pub fn active_preset(&self) -> (String, RequestTuning) {
        let name = self
            .agent
            .preset
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("balanced")
            .to_string();
        if let Some(p) = self.agent.presets.get(&name) {
            return (name, tuning_from(p));
        }
        let builtin = builtin_presets();
        if let Some(t) = builtin.get(name.as_str()) {
            return (name, t.clone());
        }
        // a mis-typed preset must not silently disable tuning
        (
            "balanced".to_string(),
            builtin.get("balanced").cloned().unwrap_or_default(),
        )
    }

    /// Preset names offered by `/effort` and Tab completion.
    pub fn preset_names(&self) -> Vec<String> {
        let mut names: Vec<String> = builtin_presets().keys().map(|s| s.to_string()).collect();
        names.extend(self.agent.presets.keys().cloned());
        names.sort();
        names.dedup();
        names
    }
}

impl From<&RequestTuning> for crate::llm::Tuning {
    fn from(t: &RequestTuning) -> Self {
        Self {
            temperature: t.temperature,
            max_tokens: t.max_tokens,
            reasoning_effort: t.reasoning_effort.clone(),
        }
    }
}

fn tuning_from(p: &PresetCfg) -> RequestTuning {
    RequestTuning {
        temperature: p.temperature,
        max_tokens: p.max_tokens,
        reasoning_effort: p.reasoning_effort.clone(),
    }
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
    /// Optional list of model ids this provider offers, shown by the
    /// `/model` picker (merged with whatever the endpoint reports live).
    pub models: Option<Vec<String>>,
    /// The endpoint needs no credential (local gateways such as Ollama,
    /// LM Studio, or Magpie). Skips the key lookup and error.
    pub no_auth: Option<bool>,
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

    pub(crate) fn default_key_env(kind: ProviderKind) -> Option<&'static str> {
        match kind {
            ProviderKind::OpenAiCompat => Some("OPENAI_API_KEY"),
            ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
            ProviderKind::Ollama => None,
        }
    }

    /// Local gateways often accept any/no credential. `no_auth = true` on the
    /// provider entry (or the ollama kind) means "do not look for a key".
    pub fn is_keyless(entry: &ProviderEntry) -> bool {
        entry.no_auth.unwrap_or(false) || entry.r#type.trim().eq_ignore_ascii_case("ollama")
    }

    /// Whether this provider needs a credential at all (false for ollama and
    /// entries marked `no_auth = true`).
    pub fn needs_auth(&self, nick: &str) -> bool {
        self.providers
            .get(nick)
            .map(|e| !Self::is_keyless(e))
            .unwrap_or(true)
    }

    /// Resolve the API key for a provider entry. Precedence: environment
    /// variable (`api_key_env` / kind default) first, then the local
    /// credential store (`kazi auth`), then an actionable error. Deliberately
    /// separate from [`Config::resolve`]: embedders plug their own key source;
    /// the CLI uses this for early, actionable failures.
    pub fn api_key_for(&self, nick: &str) -> Result<Option<String>, ConfigError> {
        let entry = self
            .providers
            .get(nick)
            .ok_or_else(|| ConfigError::Unknown(nick.to_string(), self.nicks().join(", ")))?;
        let kind = Self::kind_of(nick, entry)?;
        if kind == ProviderKind::Ollama || Self::is_keyless(entry) {
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
        if let Ok(v) = std::env::var(&env_name) {
            if !v.trim().is_empty() {
                return Ok(Some(v));
            }
        }
        // env miss → fall back to the local credential store (`kazi auth`)
        if let Ok(store) = crate::auth::load() {
            if let Some(key) = store.keys.get(nick) {
                if !key.trim().is_empty() {
                    return Ok(Some(key.clone()));
                }
            }
        }
        Err(ConfigError::BadProvider {
            nick: nick.to_string(),
            msg: format!(
                "environment variable {env_name} is not set{} and no stored key — export it, or run: kazi auth set {nick}",
                if configured { " (api_key_env in config)" } else { "" }
            ),
        })
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

        let nick = nick.ok_or(ConfigError::NoProviders)?;
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
                tuning: Tuning::from(&self.active_preset().1),
            },
        })
    }

    /// Public kind lookup for a configured nickname (e.g. `kazi auth` uses
    /// it to skip ollama providers, which need no key).
    pub fn kind_of_nick(&self, nick: &str) -> Result<ProviderKind, ConfigError> {
        let entry = self
            .providers
            .get(nick)
            .ok_or_else(|| ConfigError::Unknown(nick.to_string(), self.nicks().join(", ")))?;
        Self::kind_of(nick, entry)
    }

    /// All nicknames, for `/provider` and error messages.
    pub fn nicks(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Model ids the user listed for a provider (`models = [...]`), for the
    /// `/model` picker.
    pub fn configured_models(&self, nick: &str) -> Vec<String> {
        self.providers
            .get(nick)
            .and_then(|e| e.models.clone())
            .unwrap_or_default()
    }

    /// Fully-resolved settings (kind + key) for a nick, for out-of-band calls
    /// such as listing models. Unlike [`Config::resolve`], a missing `model`
    /// is fine — it becomes an empty string — since discovery needs no model.
    pub fn settings_for_nick(&self, nick: &str) -> Result<ProviderSettings, ConfigError> {
        let entry = self
            .providers
            .get(nick)
            .ok_or_else(|| ConfigError::Unknown(nick.to_string(), self.nicks().join(", ")))?;
        let kind = Self::kind_of(nick, entry)?;
        let api_key = self.api_key_for(nick)?;
        Ok(ProviderSettings {
            kind,
            model: entry.model.clone().unwrap_or_default(),
            base_url: entry.base_url.clone(),
            api_key,
            tuning: Tuning::from(&self.active_preset().1),
        })
    }

    /// Where the key for `nick` comes from right now: `(env_name, env_present,
    /// stored)`. Powers `/auth` and the onboarding decisions.
    pub fn key_sources(&self, nick: &str) -> Result<(String, bool, bool), ConfigError> {
        let entry = self
            .providers
            .get(nick)
            .ok_or_else(|| ConfigError::Unknown(nick.to_string(), self.nicks().join(", ")))?;
        let kind = Self::kind_of(nick, entry)?;
        let env_name = entry
            .api_key_env
            .clone()
            .or_else(|| Self::default_key_env(kind).map(str::to_string))
            .unwrap_or_default();
        let env_ok = std::env::var(&env_name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let stored = crate::auth::load()
            .map(|s| s.keys.contains_key(nick))
            .unwrap_or(false);
        Ok((env_name, env_ok, stored))
    }

    /// The first configured provider whose key is already usable (env or
    /// stored; ollama/keyless providers count as usable), skipping `exclude`.
    /// Used by first-run onboarding so a launch does not re-ask for a provider
    /// that is already ready.
    pub fn first_provider_with_key(&self, exclude: Option<&str>) -> Option<String> {
        self.nicks()
            .into_iter()
            .filter(|n| exclude != Some(n.as_str()))
            .find(|n| self.api_key_for(n).is_ok())
    }
}

/// Rewrite `<path>` so `provider = "<nick>"` is the default and the nick's own
/// `model = "..."` line carries `model`. Deliberately line-based: the user's
/// comments and layout survive (a toml round-trip would drop them).
pub fn set_default_provider(path: &Path, nick: &str, model: Option<&str>) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    // 1) top-level `provider = ...` (ignoring every `[...]` section' body)
    let mut in_table = false;
    let mut provider_set = false;
    for line in lines.iter_mut() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            in_table = true;
            continue;
        }
        if !in_table && t.split('=').next().unwrap_or("").trim() == "provider" {
            *line = format!("provider = \"{nick}\"");
            provider_set = true;
            break;
        }
    }
    if !provider_set {
        lines.insert(0, format!("provider = \"{nick}\""));
    }

    // 2) `model = ...` inside [providers.<nick>] only
    if let Some(model) = model {
        let header = format!("[providers.{nick}]");
        if let Some(start) = lines.iter().position(|l| l.trim() == header) {
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, l)| l.trim_start().starts_with('['))
                .map(|(i, _)| i)
                .unwrap_or(lines.len());
            let mut done = false;
            for line in lines.iter_mut().take(end).skip(start + 1) {
                let t = line.trim_start();
                if t.starts_with('#') {
                    continue;
                }
                if t.split('=').next().unwrap_or("").trim() == "model" {
                    *line = format!("model = \"{model}\"");
                    done = true;
                    break;
                }
            }
            if !done {
                lines.insert(start + 1, format!("model = \"{model}\""));
            }
        }
    }

    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out)
}

/// Rewrite `<path>` so the `[ui]` key carries `value` (a TOML literal, e.g.
/// `"nord"` or `true`). Line-based like [`set_default_provider`], so the user's
/// comments and ordering survive; a missing `[ui]` table is appended.
pub fn set_ui_setting(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    set_table_setting(path, "[ui]", key, value)
}

/// Same in-place rewrite for any table (`/effort` writes `[agent] preset`).
/// The table header must match exactly, so `[agent]` is never fooled by
/// `[agent.presets.deep]`.
pub fn set_table_setting(path: &Path, table: &str, key: &str, value: &str) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let ui_start = lines.iter().position(|l| l.trim_end() == table.trim());
    match ui_start {
        Some(start) => {
            let end = lines
                .iter()
                .enumerate()
                .skip(start + 1)
                .find(|(_, l)| l.trim_start().starts_with('['))
                .map(|(i, _)| i)
                .unwrap_or(lines.len());
            let existing = lines
                .iter()
                .enumerate()
                .take(end)
                .skip(start + 1)
                .find(|(_, l)| {
                    let t = l.trim_start();
                    !t.starts_with('#') && t.split('=').next().unwrap_or("").trim() == key
                })
                .map(|(i, _)| i);
            match existing {
                Some(i) => lines[i] = format!("{key} = {value}"),
                None => lines.insert(start + 1, format!("{key} = {value}")),
            }
        }
        None => {
            if lines.last().map(|l| !l.trim().is_empty()).unwrap_or(false) {
                lines.push(String::new());
            }
            lines.push(table.to_string());
            lines.push(format!("{key} = {value}"));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(path, out)
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

    #[test]
    fn ui_section_parses_and_defaults() {
        let c = cfg("[ui]\ntheme = \"light\"\ndetails = true\nbell = true\n");
        assert_eq!(c.ui.theme.as_deref(), Some("light"));
        assert_eq!(c.ui.details, Some(true));
        assert_eq!(c.ui.bell, Some(true));
        let default: Config = toml::from_str("").unwrap();
        assert!(default.ui.theme.is_none());
        assert!(default.ui.details.is_none());
        assert!(default.ui.bell.is_none(), "no surprise bells by default");
        assert!(EXAMPLE_TOML.contains("[ui]"));
    }

    #[test]
    fn provider_models_list_is_parsed_and_exposed() {
        let c = cfg("[providers.a]\ntype = \"openai\"\nmodel = \"m1\"\nmodels = [\"m1\", \"m2\"]");
        assert_eq!(
            c.configured_models("a"),
            vec!["m1".to_string(), "m2".to_string()]
        );
        // absent → empty, never an error
        assert!(c.configured_models("missing").is_empty());
    }

    #[test]
    fn settings_for_nick_needs_no_model() {
        std::env::set_var("KAZI_TEST_SETTINGS_KEY", "k");
        let c = cfg("[providers.a]\ntype = \"openai\"\napi_key_env = \"KAZI_TEST_SETTINGS_KEY\"");
        let s = c.settings_for_nick("a").unwrap();
        assert_eq!(s.kind, ProviderKind::OpenAiCompat);
        assert_eq!(s.model, "", "discovery does not require a model");
        assert_eq!(s.api_key.as_deref(), Some("k"));
    }

    #[test]
    fn set_default_provider_edits_in_place_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("kazi-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# top comment\nprovider = \"a\"\n\n[providers.a]\ntype = \"openai\"\n# keep me\nmodel = \"m1\"\nmodels = [\"m1\", \"m2\"]\n\n[providers.b]\ntype = \"openai\"\nmodel = \"m2\"\n",
        )
        .unwrap();

        set_default_provider(&path, "b", Some("m3")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# top comment"), "comments survive:\n{text}");
        assert!(text.contains("# keep me"));
        assert!(text.contains("provider = \"b\""));
        assert!(
            text.contains("models = [\"m1\", \"m2\"]"),
            "the models list is not a `model` key:\n{text}"
        );

        let parsed: Config = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("rewritten config must parse: {e}\n---\n{text}"));
        assert_eq!(parsed.provider.as_deref(), Some("b"));
        assert_eq!(parsed.providers["b"].model.as_deref(), Some("m3"));
        assert_eq!(parsed.providers["a"].model.as_deref(), Some("m1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_default_provider_inserts_the_key_when_absent() {
        let dir = std::env::temp_dir().join(format!("kazi-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[providers.z]\ntype = \"openai\"\n").unwrap();

        set_default_provider(&path, "z", None).unwrap();
        let parsed: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.provider.as_deref(), Some("z"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyless_gateway_needs_no_credential() {
        let c = cfg(
            "[providers.magpie]\ntype = \"openai\"\nbase_url = \"http://127.0.0.1:3425/v1\"\nmodel = \"m\"\nno_auth = true",
        );
        assert!(!c.needs_auth("magpie"));
        assert_eq!(c.api_key_for("magpie").unwrap(), None);
        assert_eq!(c.first_provider_with_key(None).as_deref(), Some("magpie"));
        // the model catalog path uses the same resolution
        assert_eq!(c.settings_for_nick("magpie").unwrap().api_key, None);

        let normal =
            cfg("[providers.p]\ntype = \"openai\"\napi_key_env = \"KAZI_TEST_NO_SUCH_KEY_123\"");
        std::env::remove_var("KAZI_TEST_NO_SUCH_KEY_123");
        assert!(normal.needs_auth("p"));
        assert!(
            normal.api_key_for("p").is_err(),
            "a real key is still required"
        );
    }

    #[test]
    fn set_ui_setting_creates_and_updates_the_table() {
        let dir = std::env::temp_dir().join(format!("kazi-ui-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // 1) no [ui] at all: the table is appended, existing content intact
        std::fs::write(&path, "# hello\nprovider = \"a\"\n").unwrap();
        set_ui_setting(&path, "theme", "\"nord\"").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# hello") && text.contains("[ui]"), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme.as_deref(), Some("nord"));

        // 2) an existing key is replaced in place, a new one inserted above
        set_ui_setting(&path, "theme", "\"light\"").unwrap();
        set_ui_setting(&path, "bell", "true").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("theme =").count(), 1, "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.ui.theme.as_deref(), Some("light"));
        assert_eq!(parsed.ui.bell, Some(true));

        // 3) a commented-out key is not treated as present
        std::fs::write(&path, "[ui]\n# theme = \"dark\"\ndetails = false\n").unwrap();
        set_ui_setting(&path, "theme", "\"solarized\"").unwrap();
        let parsed: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.ui.theme.as_deref(), Some("solarized"));
        assert_eq!(parsed.ui.details, Some(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preset_reaches_the_wire_and_is_written_back_without_losing_comments() {
        let dir = std::env::temp_dir().join(format!("kazi-preset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# keep this comment\n[agent]\n# and this one\nmax_rounds = 12\n\n[providers.p]\ntype = \"openai\"\nmodel = \"m\"\n",
        )
        .unwrap();

        // 1) a built-in preset is available without any config table
        let cfg: Config = toml::from_str(
            "[agent]\npreset = \"deep\"\n[providers.p]\ntype = \"openai\"\nmodel = \"m\"",
        )
        .unwrap();
        let (name, tuning) = cfg.active_preset();
        assert_eq!(name, "deep");
        assert_eq!(tuning.max_tokens, Some(8192));
        assert_eq!(tuning.reasoning_effort.as_deref(), Some("high"));
        let wire = cfg.resolve(Some("p"), None).unwrap().settings.tuning;
        assert_eq!(wire.max_tokens, Some(8192), "the preset reaches settings");

        // 2) a config-defined preset wins over the built-in of the same name
        let cfg: Config = toml::from_str(
            "[agent]\npreset = \"deep\"\n[agent.presets.deep]\nmax_tokens = 1234\n[providers.p]\ntype = \"openai\"\nmodel = \"m\"",
        )
        .unwrap();
        assert_eq!(cfg.active_preset().1.max_tokens, Some(1234));

        // 3) an unknown name falls back to balanced instead of silently
        //    disabling tuning
        let cfg: Config = toml::from_str(
            "[agent]\npreset = \"tuurbo\"\n[providers.p]\ntype = \"openai\"\nmodel = \"m\"",
        )
        .unwrap();
        assert_eq!(cfg.active_preset().0, "balanced");

        // 4) /effort writes the key back in place
        set_table_setting(&path, "[agent]", "preset", "\"deep\"").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"), "{text}");
        assert!(text.contains("# and this one"));
        assert_eq!(text.matches("preset =").count(), 1, "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.agent.preset.as_deref(), Some("deep"));
        assert_eq!(parsed.agent.max_rounds, Some(12), "other keys survive");
        // the table header must match exactly: [agent.presets.x] is not [agent]
        assert!(!text.contains("[agent.preset]"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_price_never_prints_money() {
        let cfg: Config = toml::from_str(
            r#"[usage.prices."m1"]
input = 1.0
output = 2.0
[providers.p]
type = "openai"
model = "m1""#,
        )
        .unwrap();
        let u = crate::llm::Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            cache_read_tokens: Some(1_000_000),
            cache_write_tokens: None,
        };
        let price = cfg.price_for("m1").expect("configured");
        let cost = price.cost(&u).unwrap();
        // 1M input + 0.5M output + 1M cache-read at the input price
        assert!((cost - (1.0 + 1.0 + 1.0)).abs() < 1e-9, "{cost}");

        // a model with no entry costs nothing, and says so
        assert!(cfg.price_for("m2").is_none());
        // a gateway id falls back to its last segment
        assert!(cfg.price_for("gateway/m1").is_some());
        // an EMPTY entry must not look free
        let empty: Config =
            toml::from_str("[usage.prices.m3]\n[providers.p]\ntype = \"openai\"\nmodel = \"m3\"")
                .unwrap();
        assert!(empty.price_for("m3").unwrap().cost(&u).is_none());
    }
}
