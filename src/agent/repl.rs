//! Interactive chat (`kazi chat`): streaming output, slash commands, session
//! auto-save, live provider/model switching.
//!
//! Slash commands are TABLE-DRIVEN: the single `COMMANDS` list feeds the help
//! text, TAB completion, ghost-text hints, and usage errors alike. Adding a
//! command = one table row + one match arm; nothing else to keep in sync.
//!
//! Design invariants:
//! - writing is ALWAYS autosave (`/new`, `/clear`, `/resume` only choose *where*);
//!   there is no manual `/save`, so a rename can never silently fork autosave
//! - `/clear` repoints autosave at a fresh timestamped file — it can never
//!   overwrite the session file the history came from
//!
//! Printing is ASCII (+ VT colors where harmless) so output stays readable
//! even on legacy Windows codepages (GBK/CMD).

use std::borrow::Cow;
use std::io::Write;

use async_trait::async_trait;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config as RlConfig, Context, Editor, Helper, Result as RlResult};

use crate::agent::{context::History, session, Agent, AgentError, AgentEvent};
use crate::config::Config;
use crate::tools::{checkpoint, Approver, Decision};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Undo the last task's file changes via the agent's checkpoint store.
/// Shared by the REPL and the TUI.
pub fn undo_last_task(agent: &Agent) -> std::io::Result<checkpoint::UndoOutcome> {
    match &agent.ctx.checkpointing {
        Some(cp) => cp.undo_latest(),
        None => Ok(checkpoint::UndoOutcome::Nothing),
    }
}

/// Listing cap for the undo report — a big refactor can touch many files.
const UNDO_LIST_CAP: usize = 25;

/// `/undo` — text both front-ends print.
pub fn undo_text(agent: &Agent) -> String {
    let done = match undo_last_task(agent) {
        Ok(checkpoint::UndoOutcome::Nothing) => {
            return "nothing to undo (no checkpointed task yet)".to_string()
        }
        Ok(checkpoint::UndoOutcome::AlreadyUndone) => {
            return "nothing to undo (the last task's checkpoint was already restored)".to_string()
        }
        Ok(checkpoint::UndoOutcome::Replayed(done)) => done,
        Err(e) => return format!("undo failed: {e}"),
    };
    if done.restored.is_empty() && done.removed.is_empty() && done.failed.is_empty() {
        return "nothing to undo (the last task changed no files)".to_string();
    }
    let mut out = format!(
        "restored the last task's checkpoint — {} restored, {} removed{}",
        done.restored.len(),
        done.removed.len(),
        if done.failed.is_empty() {
            String::new()
        } else {
            format!(", {} failed", done.failed.len())
        }
    );
    for (label, paths) in [
        ("restored ", &done.restored),
        ("removed  ", &done.removed),
        ("failed   ", &done.failed),
    ] {
        for p in paths.iter().take(UNDO_LIST_CAP) {
            out.push_str(&format!("\n  {label}{}", p.display()));
        }
        if paths.len() > UNDO_LIST_CAP {
            out.push_str(&format!(
                "\n  {label}… +{} more",
                paths.len() - UNDO_LIST_CAP
            ));
        }
    }
    if !done.failed.is_empty() {
        out.push_str("\n  (failed files stay undoable — /undo retries them)");
    }
    out
}

/// `/status` — provider, budget, usage, history size.
pub fn status_text(agent: &Agent) -> String {
    let u = agent.usage();
    format!(
        "provider/model: {}\nround budget:   {}\nest. context:   {} tokens\nusage(task):    in {} / out {} tokens\nmessages:       {}",
        agent.provider().describe(),
        agent.max_rounds(),
        agent.history.estimate_tokens(),
        u.input_tokens.unwrap_or(0),
        u.output_tokens.unwrap_or(0),
        agent.history.msgs_len()
    )
}

/// `/last [n]` argument: how many tool results back (default 1).
pub fn parse_back(arg: &str) -> Option<usize> {
    match arg.trim() {
        "" => Some(1),
        n => n.parse::<usize>().ok().filter(|n| *n >= 1),
    }
}

/// `/last [n]` — the newest tool output in full, `n` results back. The TUI
/// only ever shows a one-line preview; this is how you audit what the model
/// actually read.
pub fn last_tool_text(agent: &Agent, back: usize) -> String {
    const CAP_LINES: usize = 200;
    const CAP_CHARS: usize = 12_000;
    let msgs = &agent.history.msgs;

    // tool call id → tool name, taken from the assistant turns that asked
    let mut names: std::collections::HashMap<&str, &str> = Default::default();
    for m in msgs {
        for tc in &m.tool_calls {
            names.insert(tc.id.as_str(), tc.name.as_str());
        }
    }
    let tools: Vec<&crate::llm::Message> = msgs
        .iter()
        .filter(|m| m.role == crate::llm::Role::Tool)
        .collect();
    let total = tools.len();
    if total == 0 {
        return "no tool output in this session yet".to_string();
    }
    let back = back.max(1);
    if back > total {
        return format!("only {total} tool result(s) in this session");
    }
    let m = tools[total - back];
    let name = m
        .tool_call_id
        .as_deref()
        .and_then(|id| names.get(id).copied())
        .unwrap_or("tool");
    let mut text: String = m
        .text()
        .lines()
        .take(CAP_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if m.text().lines().count() > CAP_LINES {
        text.push_str("\n…[truncated]");
    }
    if text.chars().count() > CAP_CHARS {
        text = text.chars().take(CAP_CHARS).collect::<String>() + "\n…[truncated]";
    }
    format!(
        "tool output {back} of {total} — {name}{}\n{text}",
        if m.is_error { " (ERROR)" } else { "" }
    )
}

/// `/auth` — where the key for the active provider comes from, and the exact
/// command that fixes a missing one. Read-only: values are never printed.
pub fn auth_report(agent: &Agent, cfg: &Config) -> String {
    let nick = agent.nick();
    let mut out = format!("auth — provider `{nick}`\n");
    if cfg.kind_of_nick(nick).ok() == Some(crate::llm::ProviderKind::Ollama) {
        out.push_str("  local ollama provider — no key needed\n");
        return out;
    }
    match cfg.key_sources(nick) {
        Ok((env_name, env_ok, stored)) => {
            out.push_str(&format!(
                "  env {env_name}: {}\n",
                if env_ok {
                    "set ✓ (wins over a stored key)"
                } else {
                    "not set"
                }
            ));
            out.push_str(&format!(
                "  stored (kazi auth): {}\n",
                if stored { "yes ✓" } else { "no" }
            ));
            if env_ok || stored {
                out.push_str("  nothing to do — a key is available\n");
            } else {
                out.push_str(&format!(
                    "  fix:   kazi auth set {nick}\n  or:    export {env_name}=...\n  then reconnect this session: /provider {nick}\n"
                ));
            }
            if let Ok(path) = crate::auth::auth_path() {
                out.push_str(&format!("  store: {}", path.display()));
            }
        }
        Err(e) => out.push_str(&format!("  {e}\n")),
    }
    out
}

/// `/resume` with no argument: what is stored, newest first, with size and age.
pub fn sessions_text() -> String {
    let have = session::list_summaries();
    if have.is_empty() {
        return "no stored sessions yet — sessions save automatically after each task".to_string();
    }
    let mut out = String::from("stored sessions (newest first):\n");
    for s in have.iter().take(20) {
        let topic = s
            .topic
            .clone()
            .unwrap_or_default()
            .replace(['\n', '\r'], " ");
        out.push_str(&format!(
            "  {:<28} {:>4} msgs  {:>3}  {}\n",
            s.name,
            s.messages,
            age_label(s.saved_at),
            topic
        ));
    }
    if have.len() > 20 {
        out.push_str(&format!("  … +{} more\n", have.len() - 20));
    }
    out.push_str(&usage_for("/resume"));
    out
}

/// Human age label for a save timestamp (`3m`, `4h`, `2d`).
fn age_label(saved_at: u64) -> String {
    let secs = session::now_secs().saturating_sub(saved_at);
    match secs {
        0..=59 => "now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Every configured provider's candidate models as `nick/model` rows, current
/// provider first — ONE list for provider + model, so switching both is a
/// single choice. Static (config + built-in) for every provider plus a short
/// parallel live probe each, capped so opening the picker never hangs.
pub async fn all_model_choices(
    cfg: &Config,
    current_nick: Option<&str>,
    current_model: Option<&str>,
) -> Vec<String> {
    const LIVE_CAP_MS: u64 = 1_200;
    let nicks = cfg.nicks();
    let fetches = nicks.iter().cloned().map(|nick| {
        let cfg = cfg.clone();
        async move {
            let live = match cfg
                .settings_for_nick(&nick)
                .ok()
                .and_then(|s| crate::llm::build_provider(&s).ok())
            {
                Some(p) => {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(LIVE_CAP_MS),
                        p.list_models(),
                    )
                    .await
                    {
                        Ok(Ok(m)) => m,
                        _ => Vec::new(),
                    }
                }
                None => Vec::new(),
            };
            (nick, live)
        }
    });
    let live: Vec<(String, Vec<String>)> = futures::future::join_all(fetches).await;

    // current provider first, then the rest in config order
    let mut ordered: Vec<String> = Vec::new();
    if let Some(cur) = current_nick.filter(|c| nicks.iter().any(|n| n == c)) {
        ordered.push(cur.to_string());
    }
    ordered.extend(
        nicks
            .iter()
            .filter(|n| Some(n.as_str()) != current_nick)
            .cloned(),
    );

    let mut out: Vec<String> = Vec::new();
    for nick in ordered {
        let kind = cfg
            .kind_of_nick(&nick)
            .unwrap_or(crate::llm::ProviderKind::OpenAiCompat);
        let provider_model = cfg.providers.get(&nick).and_then(|e| e.model.clone());
        let active = if Some(nick.as_str()) == current_nick {
            current_model.map(str::to_string).or(provider_model.clone())
        } else {
            provider_model.clone()
        };
        let live_models = live
            .iter()
            .find(|(n, _)| n == &nick)
            .map(|(_, m)| m.clone())
            .unwrap_or_default();
        for m in crate::llm::merge_catalog(
            &cfg.configured_models(&nick),
            &live_models,
            kind,
            active.as_deref(),
        ) {
            let row = format!("{nick}/{m}");
            if !out.contains(&row) {
                out.push(row);
            }
        }
    }
    // hoist the exact current pick to the top of the list
    if let (Some(n), Some(m)) = (current_nick, current_model) {
        let cur = format!("{n}/{m}");
        if let Some(i) = out.iter().position(|r| r == &cur) {
            out.remove(i);
            out.insert(0, cur);
        }
    }
    out
}

/// The single command a partial name (`/mod`) refers to, if unambiguous — so
/// Enter on a half-typed command runs the one the user meant.
pub fn unique_prefix_command(token: &str) -> Option<String> {
    let mut hits = COMMANDS.iter().filter(|c| c.name.starts_with(token));
    let first = hits.next()?;
    if hits.next().is_some() {
        return None; // ambiguous: let the caller list the candidates
    }
    Some(first.name.to_string())
}

/// Session names matching a (possibly partial) prefix, newest first.
fn matching_sessions(prefix: &str) -> Vec<String> {
    session::list()
        .into_iter()
        .filter(|n| n.starts_with(prefix))
        .collect()
}

/// `/resume <prefix>`: load a stored session into the agent. Returns the
/// resolved session name (so the caller repoints auto-save at it) plus the
/// text both front-ends print.
pub async fn resume_text(agent: &mut Agent, prefix: &str) -> (Option<String>, String) {
    match session::resolve(prefix) {
        Some(name) => match super::session::load(&name) {
            Ok(file) => {
                let sys = file.system.or_else(|| agent.history.system.clone());
                agent.history = History::new(sys);
                agent.history.msgs = file.messages;
                let count = agent.history.msgs_len();
                (
                    Some(name.clone()),
                    format!("loaded {name} ({count} messages) — auto-save now targets `{name}`"),
                )
            }
            Err(e) => (None, format!("resume failed: {e}")),
        },
        None => {
            let matches = matching_sessions(prefix);
            let text = if matches.len() > 1 {
                format!(
                    "`{prefix}` is ambiguous — {} sessions match: {}",
                    matches.len(),
                    matches.join(", ")
                )
            } else {
                format!("no stored session matches `{prefix}` (bare /resume lists them)")
            };
            (None, text)
        }
    }
}

/// `/compact [force]` — squeeze the oldest messages into a system-prompt
/// summary. Without `force` it acts only when the context is over budget.
pub async fn compact_text(agent: &mut Agent, force: bool) -> String {
    match agent.compact(force).await {
        Ok(note) => note,
        Err(e) => format!("compact failed: {e}"),
    }
}

/// Usage hint for a command name (`/resume <prefix>` style), for both UIs.
pub fn usage_for(verb: &str) -> String {
    match spec_of(verb) {
        Some(spec) => usage_line(spec),
        None => format!("unknown command `{verb}`"),
    }
}

/// Path of the shared input-history file (used by BOTH front-ends).
pub fn history_path() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("kazi").join("history.txt"))
}

/// One history entry → one file line (multi-line tasks escape their breaks).
fn encode_history_entry(line: &str) -> String {
    line.trim().replace('\n', "\\n")
}

/// One file line → one history entry.
fn decode_history_entry(line: &str) -> String {
    line.replace("\\n", "\n")
}

/// Persisted input history, oldest first, capped to the newest entries.
/// Multi-line entries are stored with `\n` escaped so one task stays one line.
pub fn load_input_history() -> Vec<String> {
    const LIMIT: usize = 500;
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(decode_history_entry)
        .collect();
    if lines.len() > LIMIT {
        lines.drain(0..lines.len() - LIMIT);
    }
    lines
}

/// Append one submitted task to the shared history file (best-effort, no
/// consecutive duplicates, file stays bounded).
pub fn remember_input(line: &str) {
    const LIMIT: usize = 500;
    let Some(path) = history_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let entry = encode_history_entry(line);
    if entry.is_empty() {
        return;
    }
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if text.lines().next_back() == Some(entry.as_str()) {
        return;
    }
    text.push_str(&entry);
    text.push('\n');
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > LIMIT * 2 {
        text = format!("{}\n", lines[lines.len() - LIMIT..].join("\n"));
    }
    let _ = std::fs::write(&path, text);
}

/// Filesystem completion for an unfinished `@path` token: `(token_start_byte,
/// replacements)`. Directories gain a trailing `/` so completion can continue;
/// dotfiles only show up when the user typed a leading dot.
pub fn at_path_completions(line: &str, pos: usize) -> Option<(usize, Vec<String>)> {
    let head = &line[..pos.min(line.len())];
    let token_start = head.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let partial = head[token_start..].strip_prefix('@')?;
    if partial.contains(char::is_whitespace) {
        return None;
    }
    let (dir_part, name_part) = match partial.rfind(['/', '\\']) {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    let base = if dir_part.is_empty() {
        std::path::PathBuf::from(".")
    } else {
        std::path::PathBuf::from(dir_part)
    };
    let mut out: Vec<String> = Vec::new();
    for e in std::fs::read_dir(&base).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with(name_part) {
            continue;
        }
        if name.starts_with('.') && !name_part.starts_with('.') {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // platform separator: a Windows path completed with `/` reads wrong
        let suffix = if is_dir {
            std::path::MAIN_SEPARATOR.to_string()
        } else {
            String::new()
        };
        out.push(format!("@{dir_part}{name}{suffix}"));
    }
    out.sort();
    out.truncate(50);
    if out.is_empty() {
        None
    } else {
        Some((token_start, out))
    }
}

/// Apply `@path` completion to a line: a single match is inserted whole, several
/// matches extend to their longest common prefix. `None` = nothing to add.
pub fn complete_at_path(line: &str, pos: usize) -> Option<String> {
    let (start, cands) = at_path_completions(line, pos)?;
    let current = &line[start..pos.min(line.len())];
    let replacement = if cands.len() == 1 {
        cands.into_iter().next()?
    } else {
        let mut prefix = cands[0].clone();
        for c in &cands[1..] {
            let n = prefix
                .chars()
                .zip(c.chars())
                .take_while(|(a, b)| a == b)
                .count();
            prefix = prefix.chars().take(n).collect();
        }
        prefix
    };
    if replacement == current {
        return None;
    }
    Some(format!("{}{}", &line[..start], replacement))
}

/// `/provider` with no argument: what is configured, as a numbered picker.
pub fn providers_text(cfg: &Config, current: Option<&str>) -> String {
    let nicks = cfg.nicks();
    let mut out = String::from("configured providers:\n");
    for (i, nick) in nicks.iter().enumerate() {
        let model = cfg
            .providers
            .get(nick)
            .and_then(|e| e.model.clone())
            .unwrap_or_else(|| "(no model set)".to_string());
        let mark = if Some(nick.as_str()) == current {
            "  <- current"
        } else {
            ""
        };
        out.push_str(&format!("  {}) {nick:<14} model={model}{mark}\n", i + 1));
    }
    out.push_str(&usage_for("/provider"));
    out
}

/// `/model <name|#>`: a number indexes the candidate list bare `/model` prints;
/// a bare number with no usable list is an error, not a literal model name
/// (which used to be probed as a model actually called "2").
pub fn resolve_model_arg(arg: &str, models: &[String]) -> Result<String, String> {
    match arg.parse::<usize>() {
        Ok(n) => match n.checked_sub(1).and_then(|i| models.get(i)) {
            Some(m) => Ok(m.clone()),
            None => Err(format!(
                "no model #{n} in the list — run /model to see the numbered candidates"
            )),
        },
        Err(_) => Ok(arg.to_string()),
    }
}

/// Numbered model list for `/model` with no argument (the header names what the
/// rows are — `provider/model` for the combined list).
pub fn models_text(header: &str, models: &[String], current: Option<&str>) -> String {
    if models.is_empty() {
        return format!(
            "no model candidates — type a name instead ({})",
            usage_for("/model")
        );
    }
    let mut out = format!("{header}:\n");
    for (i, m) in models.iter().enumerate() {
        let mark = if Some(m.as_str()) == current {
            "  <- current"
        } else {
            ""
        };
        out.push_str(&format!("  {}) {m}{mark}\n", i + 1));
    }
    out.push_str(&usage_for("/model"));
    out
}

/// Model candidates from config `models` + the built-in fallback only — no
/// network, instant and offline-safe (seeds TAB completion at startup).
pub fn model_catalog_static(cfg: &Config, nick: &str, current: Option<&str>) -> Vec<String> {
    let kind = cfg
        .kind_of_nick(nick)
        .unwrap_or(crate::llm::ProviderKind::OpenAiCompat);
    crate::llm::merge_catalog(&cfg.configured_models(nick), &[], kind, current)
}

/// Full candidate set for the picker: config `models` ∪ a best-effort live
/// `/models` fetch (≤3s) ∪ the built-in fallback, deduped, current first.
/// Never fails — an unreachable endpoint just yields fewer candidates.
pub async fn model_catalog(cfg: &Config, nick: &str, current: Option<&str>) -> Vec<String> {
    let config_models = cfg.configured_models(nick);
    let kind = cfg
        .kind_of_nick(nick)
        .unwrap_or(crate::llm::ProviderKind::OpenAiCompat);
    let live = match cfg
        .settings_for_nick(nick)
        .ok()
        .and_then(|s| crate::llm::build_provider(&s).ok())
    {
        Some(p) => {
            match tokio::time::timeout(std::time::Duration::from_secs(3), p.list_models()).await {
                Ok(Ok(m)) => m,
                _ => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    crate::llm::merge_catalog(&config_models, &live, kind, current)
}

/// The effective provider nick for the current selection (flag > env > config).
fn effective_nick(
    cfg: &Config,
    provider: &Option<String>,
    model: &Option<String>,
) -> Option<String> {
    cfg.resolve(provider.as_deref(), model.as_deref())
        .ok()
        .map(|r| r.nick)
}

/// Resolve a `/model` or `/provider` argument that may be a 1-based index into
/// the list a bare command printed: `Some(i)` selects that item; `None` means
/// "treat the argument as a literal name".
fn listed_index(arg: &str, len: usize) -> Option<usize> {
    match arg.parse::<usize>() {
        Ok(n) if n >= 1 && n <= len => Some(n - 1),
        _ => None,
    }
}

/// `/new` and `/clear`: reset the history and pick a fresh autosave target.
/// Returns `(session name, note)` — callers store the name themselves.
pub fn start_new_session(agent: &mut Agent, topic: Option<&str>) -> (String, String) {
    agent.reset();
    let name = pick_session_name(topic);
    let note = format!("new session -> {name}");
    (name, note)
}

/// The "unknown command" answer, with typo suggestions.
pub fn unknown_command_text(verb: &str) -> String {
    let near = did_you_mean(verb);
    let hint = if near.is_empty() {
        "see /help".to_string()
    } else {
        format!("did you mean {}?", near.join(", "))
    };
    format!("unknown command `{verb}` — {hint}")
}

/// `/context`: where the estimated tokens go right now, plus what the
/// automatic machinery will do next — the numbers a user actually asks about.
pub fn context_report(agent: &Agent) -> String {
    let h = &agent.history;
    let sys = h
        .system
        .as_deref()
        .map(|s| s.chars().count() as u64 / 4)
        .unwrap_or(0);
    // messages only: `estimate_tokens()` would fold the system prompt in again
    let hist = h.estimate_msgs_tokens();
    let specs = agent.registry().specs();
    let tools: u64 = specs
        .iter()
        .map(|t| (t.name.len() + t.description.len() + t.parameters.to_string().len()) as u64 / 4)
        .sum();
    let budget = agent.max_context_tokens().max(1);
    let total = sys + hist + tools;
    let summary_blocks = h
        .system
        .as_deref()
        .map(|s| {
            s.matches("[Context summary of dropped conversation turns]")
                .count()
        })
        .unwrap_or(0);
    let summary_note = match summary_blocks {
        0 => String::new(),
        1 => " (1 compaction summary inside)".to_string(),
        n => format!(" ({n} compaction summaries inside)"),
    };
    format!(
        "context estimate:\n  system   ~{sys} tokens{summary_note}\n  history  ~{hist} tokens ({} messages)\n  tools    ~{tools} tokens ({} in catalog)\n  total    ~{total} / {budget} token budget ({}%)\n  auto-compact fires at {}% of the budget; over-budget rounds are plain-trimmed oldest-first, /compact replaces the trimmed part with a summary",
        h.msgs_len(),
        specs.len(),
        total * 100 / budget,
        Agent::compact_threshold_percent()
    )
}

/// Terminal prompt approver: `y` once, `a` always (this session, per
/// command prefix), anything else denies. A blocking stdin read is
/// deliberate here — nothing else runs while we wait.
#[derive(Default)]
pub struct StdinApprover {
    /// Command prefixes approved for the rest of this session.
    session_allows: std::sync::Mutex<Vec<String>>,
}

/// Pure answer mapping so the prompt logic is testable.
fn decision_from_input(input: &str) -> Decision {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Decision::Allow,
        "a" | "always" => Decision::AllowSession,
        "" | "n" | "no" => Decision::Deny(None),
        // anything else is a reason to hand to the model
        _ => Decision::Deny(Some(input.trim().to_string())),
    }
}

#[async_trait]
impl Approver for StdinApprover {
    async fn decide(&self, tool: &str, summary: &str) -> Decision {
        let key = crate::tools::approval_key(summary);
        if !key.is_empty()
            && self
                .session_allows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|p| p == &key)
        {
            return Decision::Allow;
        }
        print!(
            "\n{YELLOW}[{tool}] requests approval{RESET}\n{summary}\napprove? [y] once  [a] always (session)  [n] no  — or type a reason to decline with it: "
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        let decision = decision_from_input(&line);
        if decision == Decision::AllowSession && !key.is_empty() {
            self.session_allows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(key);
        }
        decision
    }
}

/// Render one agent event to stdout; shared by `chat` and `run`.
pub fn render_event(ev: &AgentEvent) {
    match ev {
        AgentEvent::TextDelta { text } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ToolStart { name } => {
            println!();
            print!("{CYAN}>> {name}{RESET}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ToolEnd {
            is_error, preview, ..
        } => {
            let (sym, col) = if *is_error { ("x", RED) } else { ("ok", GREEN) };
            println!(" {col}[{sym}]{RESET} {DIM}{preview}{RESET}");
        }
        AgentEvent::AutoCompactStarted => {
            println!();
            print!("{YELLOW}>> auto-compact{RESET}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::AutoCompactDone { note, est_tokens } => {
            println!(" {DIM}{note} (est. {est_tokens} tokens){RESET}");
        }
        AgentEvent::TodosUpdated { items } => {
            println!();
            for (content, status) in items.iter() {
                let marker = match status.as_str() {
                    "completed" => "[x]",
                    "in_progress" => "[>]",
                    _ => "[ ]",
                };
                println!("{DIM}  {marker} {content}{RESET}");
            }
        }
        AgentEvent::Feedback {
            command,
            ok,
            preview,
        } => {
            let (sym, col) = if *ok { ("ok", GREEN) } else { ("x", RED) };
            println!("\n{col}>> feedback {sym}{RESET} {DIM}`{command}` {preview}{RESET}");
        }
        AgentEvent::RoundDone { .. } => {} // classic REPL stays quiet
    }
}

// ---------------------------------------------------------------------------
// The command table — one row per command, single source of truth for
// help text, TAB completion, ghost hints, and usage errors.
// ---------------------------------------------------------------------------

/// What argument a command takes; drives TAB completion and argument hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    /// no argument
    No,
    /// optional flag word (`/compact force`)
    Flag,
    /// optional topic/name; empty → timestamped default
    Topic,
    /// stored-session prefix; TAB lists stored sessions
    Session,
    /// configured provider nick; TAB lists nicks
    Provider,
    /// free-form model name
    Model,
    /// `on | off`; empty toggles
    Toggle,
    /// permission mode name; TAB lists the three modes
    Permission,
}

/// One row of the command table.
struct CmdSpec {
    name: &'static str,
    /// Argument picture as shown in help/usage, e.g. `<prefix>`.
    args: &'static str,
    help: &'static str,
    arg: ArgKind,
}

const COMMANDS: &[CmdSpec] = &[
    CmdSpec {
        name: "/help",
        args: "",
        help: "show this help",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/undo",
        args: "",
        help: "restore files changed by the last task",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/exit",
        args: "",
        help: "save session and leave (Ctrl-D also quits)",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/quit",
        args: "",
        help: "alias for /exit",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/new",
        args: "[name]",
        help: "fresh session (optional name); the old one keeps on exit",
        arg: ArgKind::Topic,
    },
    CmdSpec {
        name: "/clear",
        args: "",
        help: "forget history; autosave moves to a fresh session file",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/compact",
        args: "[force]",
        help: "summarize old turns away (only over budget; force = always)",
        arg: ArgKind::Flag,
    },
    CmdSpec {
        name: "/resume",
        args: "<prefix>",
        help: "load a stored session (TAB lists them)",
        arg: ArgKind::Session,
    },
    CmdSpec {
        name: "/provider",
        args: "<nick|#>",
        help: "switch provider (bare /provider lists them)",
        arg: ArgKind::Provider,
    },
    CmdSpec {
        name: "/model",
        args: "<name|#>",
        help: "switch model (bare /model lists them)",
        arg: ArgKind::Model,
    },
    CmdSpec {
        name: "/yolo",
        args: "[on|off]",
        help: "toggle command auto-approval",
        arg: ArgKind::Toggle,
    },
    CmdSpec {
        name: "/permission",
        args: "[read-only|edit|yolo]",
        help: "show or set the permission mode",
        arg: ArgKind::Permission,
    },
    CmdSpec {
        name: "/context",
        args: "",
        help: "show the estimated token distribution",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/tools",
        args: "",
        help: "list built-in tools (+ MCP servers)",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/status",
        args: "",
        help: "provider, model, rounds, usage",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/last",
        args: "[n]",
        help: "show the newest tool output in full (n = how many back)",
        arg: ArgKind::Flag,
    },
    CmdSpec {
        name: "/auth",
        args: "",
        help: "where the current provider's API key comes from",
        arg: ArgKind::No,
    },
];

fn spec_of(verb: &str) -> Option<&'static CmdSpec> {
    COMMANDS.iter().find(|c| c.name == verb)
}

/// Rendered command table (`/help` output, the startup banner and the TUI
/// share this).
pub fn help_text() -> String {
    let mut out = String::from("commands:\n");
    for c in COMMANDS {
        let cmd = if c.args.is_empty() {
            c.name.to_string()
        } else {
            format!("{} {}", c.name, c.args)
        };
        out.push_str(&format!("  {cmd:<26} {}\n", c.help));
    }
    out.truncate(out.trim_end().len() + 1);
    out
}

/// Usage line for an argument-taking command; the no-argument case states
/// that instead (so `/clear foo` explains rather than a bare "unknown").
fn usage_line(spec: &CmdSpec) -> String {
    if spec.args.is_empty() {
        format!("{} takes no argument", spec.name)
    } else {
        format!("usage: {} {}", spec.name, spec.args)
    }
}

/// `Some(usage_line)` when a command that takes NO argument was given one —
/// `/undo 3` used to silently undo one task.
pub fn no_arg_violation(verb: &str, arg: &str) -> Option<String> {
    let spec = spec_of(verb)?;
    (spec.arg == ArgKind::No && !arg.is_empty()).then(|| usage_line(spec))
}

/// Completion rows for a partially typed command (`token` includes the
/// leading slash): `(name + args, help)`. Powers the TUI's `/` dropdown.
pub fn command_rows(token: &str) -> Vec<(String, String)> {
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(token))
        .map(|c| {
            let name = if c.args.is_empty() {
                c.name.to_string()
            } else {
                format!("{} {}", c.name, c.args)
            };
            (name, c.help.to_string())
        })
        .collect()
}

/// Tab completion for a partial command name: the longest common prefix of
/// the candidates, or `None` when that would not extend what was typed.
pub fn command_completion(token: &str) -> Option<String> {
    let names: Vec<&str> = COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(token))
        .map(|c| c.name)
        .collect();
    let first = *names.first()?;
    let mut prefix = first.to_string();
    for name in &names[1..] {
        let common = prefix
            .bytes()
            .zip(name.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common);
    }
    if prefix.len() > token.len() {
        Some(prefix)
    } else {
        None
    }
}

/// The command token being typed, if the input is still in command position
/// (starts with `/`, no space yet). `None` once arguments begin.
pub fn pending_command(input: &str) -> Option<&str> {
    let t = input.trim_start();
    if t.starts_with('/') && !t.contains(char::is_whitespace) {
        Some(t)
    } else {
        None
    }
}

/// Levenshtein distance (chars, no freebies) — enough for 8-char command names.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Typos → closest command names (distance ≤ 2, ties nearest-first).
fn did_you_mean(verb: &str) -> Vec<&'static str> {
    let mut scored: Vec<(usize, &'static str)> = COMMANDS
        .iter()
        .map(|c| (edit_distance(verb, c.name), c.name))
        .collect();
    scored.sort_by_key(|e| e.0);
    let best = scored[0].0;
    if best > 2 {
        Vec::new()
    } else {
        scored
            .into_iter()
            .take_while(|(d, _)| *d <= best)
            .map(|(_, n)| n)
            .take(3)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// rustyline helper: TAB completion + ghost-text hints, all driven by COMMANDS.
// ---------------------------------------------------------------------------

/// Completion + hint support for slash commands.
struct SlashHelper {
    /// Configured provider nicks (for `/provider` completion and hints).
    nicks: Vec<String>,
    /// Model as last set via `/model` (display in hints).
    model: Option<String>,
    /// Known model candidates for the current provider (`/model` completion).
    models: Vec<String>,
    yolo: bool,
}

impl Completer for SlashHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> RlResult<(usize, Vec<Pair>)> {
        if let Some(hit) = self.completions(line, pos) {
            return Ok(hit);
        }
        // `@src/age<TAB>` — attachment paths are a first-class feature, so the
        // completion must not stop at slash commands
        if let Some((start, cands)) = at_path_completions(line, pos) {
            let pairs = cands
                .into_iter()
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c,
                })
                .collect();
            return Ok((start, pairs));
        }
        Ok((pos, Vec::new()))
    }
}

impl SlashHelper {
    /// `(word_start, candidates)`, or `None` when out of scope (not a slash
    /// line, cursor mid-word ...). Empty candidates = "nothing to add" (TAB
    /// with a known arg kind still lists the full option set).
    fn completions(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let head = &line[..pos];
        if !head.starts_with('/') {
            return None;
        }
        let word_start = head.rfind(' ').map_or(0, |i| i + 1);
        let token = &head[word_start..];

        if word_start == 0 {
            // completing the verb itself
            let cands = COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(token))
                .map(|c| Pair {
                    display: format!("{:<11} {}", c.name, c.help),
                    replacement: c.name.to_string(),
                })
                .collect();
            return Some((0, cands));
        }

        let verb = line.split_whitespace().next().unwrap_or("");
        let spec = spec_of(verb)?;
        let cands = match spec.arg {
            ArgKind::Session => session::list()
                .into_iter()
                .filter(|n| n.starts_with(token))
                .map(|n| Pair {
                    display: n.clone(),
                    replacement: n,
                })
                .collect(),
            ArgKind::Provider => self
                .nicks
                .iter()
                .filter(|n| n.starts_with(token))
                .map(|n| Pair {
                    display: n.clone(),
                    replacement: n.clone(),
                })
                .collect(),
            ArgKind::Model => self
                .models
                .iter()
                .filter(|m| m.starts_with(token))
                .map(|m| Pair {
                    display: m.clone(),
                    replacement: m.clone(),
                })
                .collect(),
            ArgKind::Toggle => ["on", "off"]
                .into_iter()
                .filter(|t| t.starts_with(token))
                .map(|t| Pair {
                    display: t.to_string(),
                    replacement: t.to_string(),
                })
                .collect(),
            ArgKind::Permission => ["read-only", "edit", "yolo"]
                .into_iter()
                .filter(|t| t.starts_with(token))
                .map(|t| Pair {
                    display: t.to_string(),
                    replacement: t.to_string(),
                })
                .collect(),
            _ => Vec::new(),
        };
        Some((word_start, cands))
    }
}

impl Hinter for SlashHelper {
    type Hint = String;

    /// Ghost text right after a completed verb: the argument picture plus the
    /// options that are knowable in-process (provider nicks, yolo state).
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos != line.len() || !line.starts_with('/') {
            return None;
        }
        let verb = line.split_whitespace().next()?;
        let spec = spec_of(verb)?;
        if line.trim() != verb {
            return None; // user is already typing the argument
        }
        if spec.args.is_empty() {
            return None;
        }
        Some(match spec.arg {
            ArgKind::Provider if !self.nicks.is_empty() => {
                format!(" {} — one of: {}", spec.args, self.nicks.join(", "))
            }
            ArgKind::Model => match &self.model {
                Some(m) => format!(" {} — current: {m}", spec.args),
                None => format!(" {} — overrides the current provider's model", spec.args),
            },
            ArgKind::Session => format!(" {} — TAB lists stored sessions", spec.args),
            ArgKind::Toggle => format!(
                " {} — now {}",
                spec.args,
                if self.yolo { "on" } else { "off" }
            ),
            ArgKind::Permission => format!(" {} — one of: read-only, edit, yolo", spec.args),
            ArgKind::Flag => format!(" {}", spec.args),
            _ => format!(" {}", spec.args),
        })
    }
}

impl Highlighter for SlashHelper {
    /// Cyan command word on slash lines; plain text otherwise.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !line.starts_with('/') {
            return Cow::Borrowed(line);
        }
        let end = line.find(' ').unwrap_or(line.len());
        Cow::Owned(format!("{CYAN}{}{RESET}{}", &line[..end], &line[end..]))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{DIM}{hint}{RESET}"))
    }

    fn highlight_char(&self, line: &str, _pos: usize, _kind: CmdKind) -> bool {
        line.starts_with('/')
    }
}

impl Validator for SlashHelper {}

// Marker impl: `Helper` is just the Completer+Hinter+Highlighter+Validator
// bundle and is NOT auto-derived — every custom helper needs this line.
impl Helper for SlashHelper {}

// ---------------------------------------------------------------------------
// Loop state
// ---------------------------------------------------------------------------

/// Everything slash-commands may mutate across agent rebuilds.
pub struct ChatState {
    cfg: Config,
    /// Provider nick; `None` = follow config precedence.
    provider: Option<String>,
    model: Option<String>,
    /// Cached `/model` candidates for the current provider (bare `/model` and
    /// TAB completion share this).
    models: Vec<String>,
    mode: crate::tools::PermissionMode,
    session_name: String,
}

impl Clone for ChatState {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            models: self.models.clone(),
            mode: self.mode,
            session_name: self.session_name.clone(),
        }
    }
}

impl ChatState {
    pub fn new(
        cfg: Config,
        provider: Option<String>,
        model: Option<String>,
        mode: crate::tools::PermissionMode,
        session_name: String,
    ) -> Self {
        let models = match cfg
            .resolve(provider.as_deref(), model.as_deref())
            .ok()
            .map(|r| r.nick)
        {
            Some(nick) => model_catalog_static(&cfg, &nick, model.as_deref()),
            None => Vec::new(),
        };
        Self {
            cfg,
            provider,
            model,
            models,
            mode,
            session_name,
        }
    }

    /// (/new, /clear) points future auto-saves at a different session file.
    pub fn session_name_mut(&mut self) -> &mut String {
        &mut self.session_name
    }

    /// Initial build (once at startup): fresh tool registry + MCP servers.
    async fn rebuild(&self) -> anyhow::Result<LiveAgent> {
        Agent::default_cli_mode(
            &self.cfg,
            self.provider.as_deref(),
            self.model.as_deref(),
            self.mode,
        )
        .await
        .map(|agent| LiveAgent { agent })
    }

    /// Switch provider/model around the live agent: same tool registry (no MCP
    /// re-handshake) with the new provider/model/permission.
    async fn rebuild_from(&self, live: &LiveAgent) -> anyhow::Result<LiveAgent> {
        let approver = super::approver(self.mode == crate::tools::PermissionMode::Yolo);
        live.agent
            .rebuilt_with(
                &self.cfg,
                self.provider.as_deref(),
                self.model.as_deref(),
                approver,
                self.mode,
            )
            .await
            .map(|agent| LiveAgent { agent })
    }
}

/// A live agent plus the metadata slash-commands display.
struct LiveAgent {
    agent: Agent,
}

impl LiveAgent {
    fn describe(&self) -> String {
        self.agent.provider().describe()
    }
    fn history(&self) -> &History {
        &self.agent.history
    }
}

/// Turn a user-supplied topic into a safe file/stem name:
/// keep letters/digits (incl. CJK), spaces→`-`, drops filesystem-hostile
/// chars, trims to 32 chars, falls back to `""` when nothing survives.
pub fn slugify(topic: &str) -> String {
    let banned = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let cleaned: String = topic
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| !banned.contains(c) && !c.is_control())
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();
    let trimmed = cleaned
        .trim_matches('-')
        .chars()
        .take(32)
        .collect::<String>();
    trimmed.trim_end_matches(['-', '.', ' ']).to_string()
}

/// Pick the autosave target for a fresh session: the given slug (dotted with
/// `-2`, `-3`... on collision — never clobber an existing session file), or
/// the timestamp default.
fn pick_session_name(topic: Option<&str>) -> String {
    let base = match topic.map(slugify).filter(|s| !s.is_empty()) {
        Some(n) => n,
        None => return session::default_name(),
    };
    let mut name = base.clone();
    let mut n = 2;
    while session::load(&name).is_ok() {
        name = format!("{base}-{n}");
        n += 2;
    }
    name
}

/// Trailing-backslash continuation: `Some(text_without_backslash)` when the
/// line ends with `\` (logical-line join request, shell style). A lone `\`
/// still counts so a purely-continued block can't sneak an empty task in.
fn strip_continuation(line: &str) -> Option<&str> {
    let trimmed_end = line.trim_end();
    let body = trimmed_end.strip_suffix('\\')?;
    Some(body.trim_end())
}

/// Load a stored session into the live agent, keeping the current system
/// prompt when the file has none. Shared by the startup `--resume` path
/// and the `/resume` command.
fn load_session(live: &mut LiveAgent, name: &str) -> Result<usize, String> {
    match session::load(name) {
        Ok(f) => {
            let count = f.messages.len();
            let sys = f.system.or_else(|| live.history().system.clone());
            live.agent.history = History::new(sys);
            live.agent.history.msgs = f.messages;
            Ok(count)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Run the interactive loop; returns the process exit code.
pub async fn chat_loop(
    cfg: Config,
    provider: Option<String>,
    model: Option<String>,
    mode: crate::tools::PermissionMode,
    topic: Option<String>,
    resume: Option<String>,
) -> anyhow::Result<u8> {
    // explicit --resume wins; otherwise a fresh named session from the topic
    let session_name = match resume.as_deref() {
        Some(name) => session::resolve(name).unwrap_or_else(|| name.to_string()),
        None => pick_session_name(topic.as_deref()),
    };
    let mut state = ChatState::new(cfg, provider, model, mode, session_name.clone());
    let mut live = state.rebuild().await?;

    if let Some(name) = &resume {
        match session::resolve(name) {
            Some(n) => match load_session(&mut live, &n) {
                Ok(count) => println!("{DIM}resumed `{n}` ({count} messages){RESET}"),
                Err(e) => println!("{YELLOW}cannot resume `{n}`: {e}{RESET}"),
            },
            None => println!(
                "{YELLOW}no stored session matches `{name}` — starting a NEW session named `{name}`{RESET}"
            ),
        }
    }
    let banner_provider = live.describe();
    println!(
        "{CYAN}kazi{RESET} v{} — {banner_provider}{DIM}\nsession: {}   mode: {} — /permission to change{RESET}\n{DIM}{}{RESET}",
        env!("CARGO_PKG_VERSION"),
        state.session_name,
        state.mode.as_str(),
        help_text().trim_end()
    );

    let rl_cfg = RlConfig::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut rl: Editor<SlashHelper, DefaultHistory> = Editor::with_config(rl_cfg)?;
    // shared, persisted history: ↑ after a restart recalls the last tasks
    for entry in load_input_history() {
        let _ = rl.add_history_entry(&entry);
    }
    rl.set_helper(Some(SlashHelper {
        nicks: state.cfg.nicks(),
        model: state.model.clone(),
        models: state.models.clone(),
        yolo: state.mode == crate::tools::PermissionMode::Yolo,
    }));

    loop {
        // the mode rides in the prompt: a `--yolo` session must never look
        // like a prompting one
        let prompt = match state.mode {
            crate::tools::PermissionMode::ReadOnly => "kazi[ro]> ",
            crate::tools::PermissionMode::Yolo => "kazi[yolo]> ",
            crate::tools::PermissionMode::Edit => "kazi> ",
        };
        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(e) => {
                println!("{YELLOW}input error: {e}{RESET}");
                break;
            }
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // Continuation lines: a trailing backslash swallows the newline and
        // keeps reading; history stores the JOINED text so ↑ recalls the
        // whole logical line.
        let mut line = line;
        while let Some(without_escape) = strip_continuation(&line) {
            let next = match rl.readline("kazi… ") {
                Ok(l) => l,
                Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                    // abort the entry entirely — nothing half-submitted
                    println!("{DIM}(input cancelled){RESET}");
                    line = String::new();
                    break;
                }
                Err(e) => {
                    println!("{YELLOW}input error: {e}{RESET}");
                    line = String::new();
                    break;
                }
            };
            let next = next.trim();
            if next.is_empty() {
                line = without_escape.to_string();
                break;
            }
            line = format!("{without_escape}\n{next}");
        }
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(&line);
        remember_input(&line);

        if line.starts_with('/') {
            // pass the full line (with '/') — match arms key on the slash form
            if handle_command(&line, &mut state, &mut live).await.is_exit() {
                break;
            }
            // keep completion hints honest with whatever the command changed
            if let Some(h) = rl.helper_mut() {
                h.yolo = state.mode == crate::tools::PermissionMode::Yolo;
                h.model = state.model.clone();
                h.models = state.models.clone();
                h.nicks = state.cfg.nicks();
            }
            continue;
        }

        // A real task: expand @file references, stream it, then auto-save.
        println!();
        let task = crate::agent::expand_file_refs(&line, &live.agent.ctx.cwd);
        match live.agent.run_task(&task, |e| render_event(&e)).await {
            Ok(_) => {
                println!();
                let _ = session::save(
                    &state.session_name,
                    live.history().system.as_deref(),
                    &live.history().msgs,
                );
            }
            Err(AgentError::RoundsExceeded(r)) => {
                println!("\n{YELLOW}stopped after {r} rounds — narrow the task or raise agent.max_rounds{RESET}");
                let _ = session::save(
                    &state.session_name,
                    live.history().system.as_deref(),
                    &live.history().msgs,
                );
            }
            Err(AgentError::Provider(e)) => {
                let text = live.agent.error_text(&AgentError::Provider(e));
                println!("\n{RED}{text}{RESET}");
                let _ = session::save(
                    &state.session_name,
                    live.history().system.as_deref(),
                    &live.history().msgs,
                );
            }
            Err(AgentError::Cancelled) => {
                println!("\n{DIM}cancelled{RESET}");
                let _ = session::save(
                    &state.session_name,
                    live.history().system.as_deref(),
                    &live.history().msgs,
                );
            }
        }
    }

    // empty sessions (no messages) never touch the disk
    if live.history().msgs_len() == 0 {
        return Ok(0);
    }
    let _ = session::save(
        &state.session_name,
        live.history().system.as_deref(),
        &live.history().msgs,
    );
    println!("{DIM}session saved -> {}{RESET}", state.session_name);
    Ok(0)
}

enum Flow {
    Continue,
    Exit,
}

impl Flow {
    fn is_exit(&self) -> bool {
        matches!(self, Flow::Exit)
    }
}

async fn handle_command(cmd: &str, state: &mut ChatState, live: &mut LiveAgent) -> Flow {
    let verb = cmd.split_once(' ').map(|(a, _)| a).unwrap_or(cmd);
    let arg = cmd
        .split_once(' ')
        .map(|(_, b)| b.trim())
        .unwrap_or("")
        .to_string();
    // `/Undo` must behave like `/undo`: the table is lowercase
    let verb_key = verb.to_ascii_lowercase();
    // a half-typed command (`/mod`) completes to its unique match: Enter means
    // "run the one I clearly meant"
    let verb_key = match spec_of(&verb_key) {
        Some(_) => verb_key,
        None => unique_prefix_command(&verb_key).unwrap_or(verb_key),
    };

    // `/undo 3` and friends: explain instead of silently ignoring the argument
    if let Some(usage) = no_arg_violation(&verb_key, &arg) {
        println!("{YELLOW}{usage}{RESET}");
        return Flow::Continue;
    }

    match (verb_key.as_str(), arg.as_str()) {
        ("/help", _) => println!("{}", help_text()),
        ("/undo", _) => println!("{}", undo_text(&live.agent)),
        ("/context", _) => println!("{}", context_report(&live.agent)),
        ("/exit", _) | ("/quit", _) => return Flow::Exit,
        ("/clear", _) => {
            let (name, note) = start_new_session(&mut live.agent, None);
            *state.session_name_mut() = name;
            println!("{GREEN}history cleared{RESET} — {note}");
        }
        ("/new", topic) => {
            let (name, note) = start_new_session(
                &mut live.agent,
                if topic.is_empty() { None } else { Some(topic) },
            );
            *state.session_name_mut() = name;
            println!("{note}");
        }
        ("/compact", arg) => {
            let force = match arg {
                "" => false,
                "force" | "--force" | "f" => true,
                _ => {
                    println!(
                        "{YELLOW}{}{RESET}",
                        usage_line(spec_of("/compact").unwrap())
                    );
                    return Flow::Continue;
                }
            };
            println!("{}", compact_text(&mut live.agent, force).await);
        }
        ("/status", _) => println!("{}", status_text(&live.agent)),
        ("/last", arg) => match parse_back(arg) {
            Some(back) => println!("{}", last_tool_text(&live.agent, back)),
            None => println!("{YELLOW}{}{RESET}", usage_line(spec_of("/last").unwrap())),
        },
        ("/auth", _) => println!("{}", auth_report(&live.agent, &state.cfg)),
        ("/tools", _) => println!("{}", live.agent.registry().catalog()),
        ("/resume", prefix) => {
            if prefix.is_empty() {
                println!("{}", sessions_text());
            } else {
                // resolve before printing: auto-save must follow the session
                // we just loaded, not the one this chat started in
                let (name, text) = resume_text(&mut live.agent, prefix).await;
                if let Some(name) = name {
                    *state.session_name_mut() = name;
                }
                println!("{text}");
            }
        }
        ("/provider", arg) => {
            if arg.is_empty() {
                let current = effective_nick(&state.cfg, &state.provider, &state.model);
                println!("{}", providers_text(&state.cfg, current.as_deref()));
            } else {
                let nicks = state.cfg.nicks();
                let nick = match listed_index(arg, nicks.len()) {
                    Some(i) => nicks[i].clone(),
                    None => arg.to_string(),
                };
                let mut next = state.clone();
                next.provider = Some(nick);
                // a model override from the old provider may not exist on the
                // new one — drop it and the now-stale candidate cache
                next.model = None;
                next.models.clear();
                match next.rebuild_from(live).await {
                    Ok(mut new) => {
                        // a provider switch changes the wire, not the conversation
                        new.agent.history =
                            std::mem::replace(&mut live.agent.history, History::new(None));
                        println!(
                            "switched -> {}{DIM}\n(/model picks provider and model in one step){RESET}",
                            new.describe()
                        );
                        *state = next;
                        *live = new;
                    }
                    Err(e) => println!("{RED}switch failed: {e}{RESET}"),
                }
            }
        }
        ("/model", arg) => {
            if arg.is_empty() {
                let nick = effective_nick(&state.cfg, &state.provider, &state.model);
                let current = state.model.clone().or_else(|| {
                    state
                        .cfg
                        .resolve(state.provider.as_deref(), None)
                        .ok()
                        .map(|r| r.settings.model)
                });
                println!("{DIM}checking the configured providers for their model lists…{RESET}");
                state.models =
                    all_model_choices(&state.cfg, nick.as_deref(), current.as_deref()).await;
                let current_row = match (nick.as_deref(), current.as_deref()) {
                    (Some(n), Some(m)) => Some(format!("{n}/{m}")),
                    _ => None,
                };
                println!(
                    "{}",
                    models_text(
                        "providers / models (one step: pick a row to switch both)",
                        &state.models,
                        current_row.as_deref()
                    )
                );
            } else {
                // a number indexes the list bare `/model` printed; a bad number
                // is an error, never a literal model name
                let chosen = match resolve_model_arg(arg, &state.models) {
                    Ok(m) => m,
                    Err(msg) => {
                        println!("{YELLOW}{msg}{RESET}");
                        return Flow::Continue;
                    }
                };
                // a combined `nick/model` row switches provider AND model
                let (provider, model) = match chosen.split_once('/') {
                    Some((nick, m)) if state.cfg.providers.contains_key(nick) => {
                        (Some(nick.to_string()), m.to_string())
                    }
                    _ => (state.provider.clone(), chosen.clone()),
                };
                let mut next = state.clone();
                next.provider = provider;
                next.model = Some(model);
                match next.rebuild_from(live).await {
                    Ok(mut new) => {
                        // Dry-run before committing: a wrong model name only
                        // fails at the next API call otherwise (HTTP 4xx with
                        // context now gone). One tiny probe round verifies the
                        // model exists on this provider; failure keeps the
                        // current live agent untouched.
                        match new.agent.probe().await {
                            Ok(()) => {
                                // keep the conversation: switching the model
                                // must not silently start from scratch
                                new.agent.history = std::mem::replace(
                                    &mut live.agent.history,
                                    History::new(None),
                                );
                                println!("switched -> {}", new.describe());
                                *state = next;
                                *live = new;
                            }
                            Err(e) => println!(
                                "{RED}model rejected by provider, staying on {}{RESET}\n{DIM}{e}{RESET}",
                                live.describe()
                            ),
                        }
                    }
                    Err(e) => println!("{RED}switch failed: {e}{RESET}"),
                }
            }
        }
        ("/yolo", on) => {
            use crate::tools::PermissionMode;
            let next = match on {
                "" => {
                    if state.mode == PermissionMode::Yolo {
                        PermissionMode::Edit
                    } else {
                        PermissionMode::Yolo
                    }
                }
                "on" | "1" | "true" => PermissionMode::Yolo,
                "off" | "0" | "false" => PermissionMode::Edit,
                _ => {
                    println!("{YELLOW}{}{RESET}", usage_line(spec_of("/yolo").unwrap()));
                    return Flow::Continue;
                }
            };
            state.mode = next;
            // flip the shared permission cell in place: rebuilding the agent
            // here used to throw the whole conversation away
            live.agent.ctx.permission.set(next);
            println!(
                "permission mode: {GREEN}{}{RESET} ({})",
                state.mode.as_str(),
                if state.mode == PermissionMode::Yolo {
                    "run_command executes without prompts"
                } else {
                    "run_command asks for approval"
                }
            );
        }
        ("/permission", arg) => {
            use crate::tools::PermissionMode;
            let next = if arg.is_empty() {
                None
            } else {
                match PermissionMode::parse(arg) {
                    Some(m) => Some(m),
                    None => {
                        println!(
                            "{YELLOW}{}{RESET}",
                            usage_line(spec_of("/permission").unwrap())
                        );
                        return Flow::Continue;
                    }
                }
            };
            match next {
                None => println!(
                    "permission mode: {}(read-only = inspect only, edit = writes allowed, yolo = no prompts)",
                    state.mode.as_str()
                ),
                Some(m) => {
                    state.mode = m;
                    live.agent.ctx.permission.set(m); // in place: keep the history
                    println!("permission mode -> {}", state.mode.as_str());
                }
            }
        }
        _ => println!("{YELLOW}{}{RESET}", unknown_command_text(verb)),
    }
    Flow::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_entries_roundtrip_multiline_tasks() {
        let task = "line one\nline two";
        let encoded = encode_history_entry(task);
        assert_eq!(encoded, "line one\\nline two");
        assert_eq!(decode_history_entry(&encoded), task);
        // blank submissions never enter the history
        assert_eq!(encode_history_entry("   "), "");
    }

    #[test]
    fn at_path_completions_offer_attachment_paths() {
        let dir = std::env::temp_dir().join(format!("kazi-at-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for f in ["alpha.txt", "alpine.txt", "beta.txt"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let base = dir.display().to_string();
        let sep = std::path::MAIN_SEPARATOR;

        let line = format!("look at @{base}{sep}al");
        let (start, cands) = at_path_completions(&line, line.len()).expect("in scope");
        assert_eq!(start, 8, "token starts after the space");
        assert!(cands.iter().any(|c| c.ends_with("alpha.txt")), "{cands:?}");
        assert!(cands.iter().any(|c| c.ends_with("alpine.txt")), "{cands:?}");
        assert!(!cands.iter().any(|c| c.ends_with("beta.txt")), "{cands:?}");

        // a unique match completes whole, several extend to the common prefix
        let line = format!("read @{base}{sep}beta");
        assert_eq!(
            complete_at_path(&line, line.len()),
            Some(format!("read @{base}{sep}beta.txt"))
        );
        let line = format!("read @{base}{sep}al");
        assert_eq!(
            complete_at_path(&line, line.len()),
            Some(format!("read @{base}{sep}alp"))
        );

        // directories keep the separator so completion can continue
        let line = format!("look @{base}{sep}su");
        let done = complete_at_path(&line, line.len()).expect("dir match");
        assert!(done.ends_with(&format!("sub{sep}")), "{done}");

        // no @ token → not our business
        assert!(complete_at_path("no refs here", 12).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_commands_complete_to_their_unique_match() {
        assert_eq!(unique_prefix_command("/mod").as_deref(), Some("/model"));
        assert_eq!(unique_prefix_command("/resu").as_deref(), Some("/resume"));
        // ambiguous or unknown prefixes stay with the caller to explain
        assert_eq!(unique_prefix_command("/co"), None);
        assert_eq!(unique_prefix_command("/zzz"), None);
    }

    #[tokio::test]
    async fn all_model_choices_prefix_rows_with_the_provider() {
        // no usable key → no live fetch, so this stays offline and instant
        let cfg: Config = toml::from_str(
            "[providers.a]\ntype = \"openai\"\nmodel = \"m1\"\napi_key_env = \"KAZI_TEST_NOPE_A\"\n[providers.b]\ntype = \"openai\"\nmodel = \"m2\"\napi_key_env = \"KAZI_TEST_NOPE_B\"",
        )
        .unwrap();
        std::env::remove_var("KAZI_TEST_NOPE_A");
        std::env::remove_var("KAZI_TEST_NOPE_B");

        let rows = all_model_choices(&cfg, Some("b"), Some("m2")).await;
        assert!(
            rows.iter()
                .all(|r| r.starts_with("a/") || r.starts_with("b/")),
            "{rows:?}"
        );
        assert_eq!(rows[0], "b/m2", "the current pick is hoisted: {rows:?}");
        assert!(rows.iter().any(|r| r == "a/m1"), "{rows:?}");
    }

    #[test]
    fn model_arg_resolves_numbers_or_errors() {
        let models = vec!["a".to_string(), "b".to_string()];
        assert_eq!(resolve_model_arg("2", &models).unwrap(), "b");
        assert_eq!(resolve_model_arg("gpt-4o", &models).unwrap(), "gpt-4o");
        assert!(resolve_model_arg("5", &models).is_err(), "out of range");
        assert!(resolve_model_arg("0", &models).is_err(), "1-based");
        assert!(resolve_model_arg("1", &[]).is_err(), "nothing cached");
    }

    #[test]
    fn providers_text_marks_the_current_one() {
        let cfg: Config = toml::from_str(
            "[providers.a]\ntype = \"openai\"\nmodel = \"m1\"\n[providers.b]\ntype = \"anthropic\"\nmodel = \"m2\"",
        )
        .unwrap();
        let text = providers_text(&cfg, Some("b"));
        assert!(text.contains("2) b"), "{text}");
        assert!(text.contains("<- current"), "{text}");
        assert_eq!(text.matches("<- current").count(), 1, "exactly one: {text}");
    }

    #[test]
    fn last_tool_text_walks_back_through_tool_results() {
        use crate::llm::{Message, ToolCall};
        let provider: std::sync::Arc<dyn crate::llm::Provider> =
            std::sync::Arc::new(crate::llm::mock::MockProvider::new(vec![]));
        let ctx = crate::tools::ToolCtx {
            cwd: std::env::temp_dir(),
            sandbox_root: None,
            timeout: std::time::Duration::from_secs(1),
            output_limit: 1024,
            allow_commands: vec![],
            approver: std::sync::Arc::new(crate::tools::AutoApprover),
            checkpointing: None,
            todos: None,
            permission: crate::tools::Permission::default(),
            subagent: None,
        };
        let mut agent = Agent::new(
            provider,
            crate::tools::ToolRegistry::builtin(),
            ctx,
            Some("sys".into()),
            4,
            1_000,
        );
        assert!(last_tool_text(&agent, 1).contains("no tool output"));

        agent.history.push(Message::assistant_with_calls(
            None,
            vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "x"}),
            }],
        ));
        agent
            .history
            .push(Message::tool_result("c1", "line one\nline two", false));

        let text = last_tool_text(&agent, 1);
        assert!(text.contains("read_file"), "{text}");
        assert!(
            text.contains("line two"),
            "full output, not a preview: {text}"
        );
        assert!(last_tool_text(&agent, 2).contains("only 1 tool result"));
        assert_eq!(parse_back(""), Some(1));
        assert_eq!(parse_back("3"), Some(3));
        assert_eq!(parse_back("0"), None);
        assert_eq!(parse_back("nope"), None);
    }

    #[test]
    fn continuation_backslash_detected() {
        assert_eq!(
            strip_continuation("refactor this \\"),
            Some("refactor this")
        );
        assert_eq!(
            strip_continuation("a\\\\b"),
            None,
            "escaped backslash, not EOL"
        );
        // terminal backslash with spaces before it still joins
        assert_eq!(strip_continuation("x  \\  "), Some("x"));
        assert_eq!(strip_continuation("plain"), None);
        assert_eq!(strip_continuation("\\"), Some(""));
    }

    #[test]
    fn table_feeds_complete_help() {
        let h = help_text();
        for c in COMMANDS {
            assert!(h.contains(c.name), "help missing {}", c.name);
            assert!(h.contains(c.help), "help missing desc of {}", c.name);
        }
        // names unique
        let mut names: Vec<_> = COMMANDS.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), COMMANDS.len());
    }

    #[test]
    fn usage_lines_match_args_hints() {
        assert!(usage_line(spec_of("/resume").unwrap()).contains("<prefix>"));
        assert!(usage_line(spec_of("/clear").unwrap()).contains("no argument"));
    }

    #[test]
    fn did_you_mean_scores_typos() {
        let near = did_you_mean("/resum"); // dropped tail
        assert!(near.contains(&"/resume"));
        // far-away typo → no noise
        assert!(did_you_mean("/zzzzzzzzzz").is_empty());
    }

    #[test]
    fn completions_cover_verbs_then_args() {
        let helper = SlashHelper {
            nicks: vec!["deepseek".into(), "ollama".into()],
            model: None,
            models: vec!["deepseek-chat".into(), "deepseek-reasoner".into()],
            yolo: false,
        };
        // verb completion at position 0
        let (start, cands) = helper.completions("/res", 4).expect("in scope");
        assert_eq!(start, 0);
        assert_eq!(cands[0].replacement, "/resume");
        // arg completion: provider nicks after "/provider "
        let (start, cands) = helper.completions("/provider de", 12).expect("in scope");
        assert_eq!(start, 10);
        assert_eq!(cands[0].replacement, "deepseek");
        // TAB on a known arg kind lists everything even with empty token
        let (_, cands) = helper.completions("/provider ", 10).expect("in scope");
        assert_eq!(cands.len(), 2);
        // model completion from the cached candidates
        let (start, cands) = helper
            .completions("/model deepseek-", 16)
            .expect("in scope");
        assert_eq!(start, 7);
        assert_eq!(cands.len(), 2);
        // toggle offers on/off
        let (_, cands) = helper.completions("/yolo o", 7).expect("in scope");
        assert_eq!(cands.len(), 2);
        // non-slash line: out of scope
        assert!(helper.completions("list files", 10).is_none());
        // no model candidates cached → empty, still in scope
        let bare = SlashHelper {
            nicks: vec![],
            model: None,
            models: vec![],
            yolo: false,
        };
        let (_, cands) = bare.completions("/model m", 8).expect("in scope");
        assert!(cands.is_empty());
    }

    #[test]
    fn hints_show_argument_picture_and_options() {
        let helper = SlashHelper {
            nicks: vec!["a".into()],
            model: Some("m1".into()),
            models: vec![],
            yolo: true,
        };
        let history = DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        let hint = helper.hint("/model", 6, &ctx).expect("hint");
        assert!(hint.contains("<name|#>") && hint.contains("m1"));
        let hint = helper
            .hint("/provider ", 10, &ctx)
            .expect("hint after space");
        assert!(hint.contains("one of: a"));
        // mid-line cursor or past the verb → silent
        assert!(helper.hint("/model gpt", 10, &ctx).is_none());
        assert!(helper.hint("plain text", 10, &ctx).is_none());
    }

    #[test]
    fn listed_index_selects_by_number_or_name() {
        assert_eq!(listed_index("2", 3), Some(1));
        assert_eq!(listed_index("1", 1), Some(0));
        assert_eq!(listed_index("0", 3), None, "1-based: 0 is not an index");
        assert_eq!(listed_index("4", 3), None, "out of range → literal name");
        assert_eq!(listed_index("gpt-4o", 3), None);
        assert_eq!(listed_index("", 3), None);
    }

    #[test]
    fn models_text_marks_current_and_handles_empty() {
        let models = vec!["a".to_string(), "b".to_string()];
        let t = models_text("providers / models", &models, Some("b"));
        assert!(t.contains("providers / models:"), "{t}");
        assert!(t.contains("1) a"), "{t}");
        assert!(t.contains("2) b  <- current"), "{t}");
        assert!(models_text("x", &[], None).contains("no model candidates"));
    }

    #[test]
    fn model_catalog_static_merges_config_and_defaults() {
        let cfg: Config = toml::from_str(
            "[providers.anthropic]\ntype = \"anthropic\"\nmodel = \"claude-sonnet-4-5\"\nmodels = [\"claude-x\"]",
        )
        .unwrap();
        let c = model_catalog_static(&cfg, "anthropic", Some("claude-sonnet-4-5"));
        assert_eq!(c[0], "claude-sonnet-4-5", "current first");
        assert!(c.contains(&"claude-x".to_string()), "config list merged");
        assert!(c.len() > 2, "built-in fallback merged too: {c:?}");
    }

    #[test]
    fn pick_session_name_never_clobbers() {
        let base = format!("pick-test-{}", std::process::id());
        session::save(&base, None, &[]).unwrap();
        let name = pick_session_name(Some(&base));
        assert_ne!(name, base, "existing session must not be reused");
        assert!(name.starts_with(&format!("{base}-")));
        std::fs::remove_file(
            session::sessions_dir()
                .unwrap()
                .join(format!("{base}.json")),
        )
        .unwrap();
    }
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn command_completion_and_dropdown_rows() {
        // command position detection
        assert_eq!(pending_command("/hel"), Some("/hel"));
        assert!(pending_command("/help me").is_none(), "arguments end it");
        assert!(pending_command("plain task").is_none());
        assert!(pending_command("ask about /undo").is_none());

        // rows for `/` cover the whole table and carry help text
        let all = command_rows("/");
        assert!(all.len() >= 10, "got {} rows", all.len());
        assert!(all.iter().any(|(n, _)| n == "/resume <prefix>"), "{all:?}");
        assert!(all.iter().any(|(_, help)| help.contains("restore files")));

        // unique prefix completes all the way
        assert_eq!(command_completion("/res").as_deref(), Some("/resume"));
        // a fork that shares only what was typed makes no progress
        assert_eq!(command_completion("/co"), None);
        assert_eq!(command_completion("/"), None);
        // already-complete name: nothing to add
        assert_eq!(command_completion("/undo"), None);
    }

    #[test]
    fn approval_answers_map_to_decisions() {
        assert_eq!(decision_from_input("y\n"), Decision::Allow);
        assert_eq!(decision_from_input("Yes"), Decision::Allow);
        assert_eq!(decision_from_input(" a "), Decision::AllowSession);
        assert_eq!(decision_from_input("always\n"), Decision::AllowSession);
        assert_eq!(decision_from_input("n\n"), Decision::Deny(None));
        assert_eq!(decision_from_input(""), Decision::Deny(None));
        // a free-text answer becomes a reason the model can act on
        assert_eq!(
            decision_from_input("use --verbose instead\n"),
            Decision::Deny(Some("use --verbose instead".into()))
        );
    }
    use super::slugify;

    #[test]
    fn keeps_cjk_and_words_with_dash() {
        assert_eq!(slugify("修 kproxy 的代理问题"), "修-kproxy-的代理问题");
        assert_eq!(slugify("fix kproxy proxy"), "fix-kproxy-proxy");
    }

    #[test]
    fn strips_filesystem_hostile_chars() {
        assert_eq!(slugify("fix:kproxy/proxy?*"), "fixkproxyproxy");
        assert_eq!(slugify("a<b>c\"d"), "abcd");
        assert_eq!(slugify("x\\y|z"), "xyz");
    }

    #[test]
    fn empty_and_junk_topics_fall_back() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("///:"), "");
        assert_eq!(slugify(" leading space "), "leading-space");
    }

    #[test]
    fn truncates_to_32_chars() {
        let long = "abcdefgh".repeat(6);
        assert_eq!(slugify(&long).chars().count(), 32);
        assert!(!slugify(&long).ends_with('-'));
    }
}
