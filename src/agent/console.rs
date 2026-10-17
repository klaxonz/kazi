//! Shared chat front-end support: the slash-command table, the plain-stdout
//! event renderer (`kazi run`'s progress output), the terminal stdin approver,
//! persisted input history, `@path` completion, and provider/model catalogs.
//!
//! Slash commands are TABLE-DRIVEN: the single `COMMANDS` list feeds the help
//! text, TAB completion, ghost-text hints, and usage errors alike. Adding a
//! command = one table row + the one TUI match arm that acts on it; nothing
//! else to keep in sync.
//!
//! Design invariants:
//! - writing is ALWAYS autosave (`/new`, `/clear`, `/resume` only choose *where*);
//!   there is no manual `/save`, so a rename can never silently fork autosave
//! - `/clear` repoints autosave at a fresh timestamped file — it can never
//!   overwrite the session file the history came from
//!
//! Printing is ASCII (+ VT colors where harmless) so output stays readable
//! even on legacy Windows codepages (GBK/CMD).

use std::io::Write;

use async_trait::async_trait;

use crate::agent::{context::History, session, Agent, AgentEvent};
use crate::config::Config;
use crate::tools::{checkpoint, Approver, Decision};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Undo the last task's file changes via the agent's checkpoint store.
/// Executed by the TUI's `/undo`.
pub fn undo_last_task(agent: &Agent) -> std::io::Result<checkpoint::UndoOutcome> {
    match &agent.ctx.checkpointing {
        Some(cp) => cp.undo_latest(),
        None => Ok(checkpoint::UndoOutcome::Nothing),
    }
}

/// Listing cap for the undo report — a big refactor can touch many files.
const UNDO_LIST_CAP: usize = 25;

/// `/undo` — the report the TUI prints into its transcript.
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

/// Merge candidate rows for every configured provider into `nick/model` rows,
/// current provider first, the current pick hoisted. `live` carries
/// per-provider live model lists (empty slice = static candidates only).
fn merge_choice_rows(
    cfg: &Config,
    current_nick: Option<&str>,
    current_model: Option<&str>,
    live: &[(String, Vec<String>)],
) -> Vec<String> {
    let nicks = cfg.nicks();

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

/// The INSTANT candidate list: config `models` + built-in fallback only, no
/// network. This is what a picker opens with — waiting for `/models` before
/// drawing the list is what made `/model` feel like a hang.
pub fn model_choices_static(
    cfg: &Config,
    current_nick: Option<&str>,
    current_model: Option<&str>,
) -> Vec<String> {
    merge_choice_rows(cfg, current_nick, current_model, &[])
}

/// The same list with live `/models` results merged in: every configured
/// provider is probed in parallel with a short cap, so this costs at most one
/// cap's worth of latency and never blocks on a dead endpoint.
pub async fn all_model_choices(
    cfg: &Config,
    current_nick: Option<&str>,
    current_model: Option<&str>,
) -> Vec<String> {
    const LIVE_CAP_MS: u64 = 1_200;
    let fetches = cfg.nicks().into_iter().map(|nick| {
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
    merge_choice_rows(cfg, current_nick, current_model, &live)
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

/// Session names matching a (possibly partial) prefix, newest first.
fn matching_sessions(prefix: &str) -> Vec<String> {
    session::list()
        .into_iter()
        .filter(|n| n.starts_with(prefix))
        .collect()
}

/// `/resume <prefix>`: load a stored session into the agent. Returns the
/// resolved session name (so the caller repoints auto-save at it) plus the
/// text the TUI prints.
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

/// Usage hint for a command name (`/resume <prefix>` style), for the TUI.
pub fn usage_for(verb: &str) -> String {
    match spec_of(verb) {
        Some(spec) => usage_line(spec),
        None => format!("unknown command `{verb}`"),
    }
}

/// Path of the persisted input-history file.
fn history_path() -> Option<std::path::PathBuf> {
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

/// Render one agent event to plain stdout — `kazi run`'s streaming output
/// (headless progress; the TUI renders its own events).
pub fn render_event(ev: &AgentEvent) {
    match ev {
        AgentEvent::TextDelta { text } => {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ToolStart { name, summary, .. } => {
            println!();
            if summary.is_empty() {
                print!("{CYAN}>> {name}{RESET}");
            } else {
                print!("{CYAN}>> {name}{RESET} {DIM}{summary}{RESET}");
            }
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ToolEnd {
            is_error,
            preview,
            diff,
            ..
        } => {
            let (sym, col) = if *is_error { ("x", RED) } else { ("ok", GREEN) };
            println!(" {col}[{sym}]{RESET} {DIM}{preview}{RESET}");
            if let Some(d) = diff {
                // headless runs stay terse: one coloured line per changed row
                for line in d.lines().take(40) {
                    let colour = if line.starts_with('+') {
                        GREEN
                    } else if line.starts_with('-') {
                        RED
                    } else {
                        DIM
                    };
                    println!("{colour}{line}{RESET}");
                }
            }
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
        AgentEvent::RoundDone { .. } => {} // run stays quiet on round progress
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
        help: "save session and leave (Ctrl+C also quits)",
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
        help: "load a stored session (bare opens the browser)",
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
    CmdSpec {
        name: "/theme",
        args: "[name]",
        help: "switch the colour theme (dark | light | mono)",
        arg: ArgKind::Flag,
    },
    CmdSpec {
        name: "/details",
        args: "[on|off]",
        help: "expand or collapse full tool output",
        arg: ArgKind::Toggle,
    },
    CmdSpec {
        name: "/sessions",
        args: "",
        help: "browse saved sessions (Enter resumes one)",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/export",
        args: "[path]",
        help: "write the conversation to a markdown file",
        arg: ArgKind::Topic,
    },
    CmdSpec {
        name: "/copy",
        args: "[all]",
        help: "copy the last answer (or the whole transcript) to the clipboard",
        arg: ArgKind::Flag,
    },
    CmdSpec {
        name: "/retry",
        args: "",
        help: "run the last task again",
        arg: ArgKind::No,
    },
];

fn spec_of(verb: &str) -> Option<&'static CmdSpec> {
    COMMANDS.iter().find(|c| c.name == verb)
}

/// Rendered command table (`/help` output; the startup banner in the TUI
/// shares this).
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
        // the instant list (what a picker opens with) is identical when there is
        // no live data to merge — and it needs no network at all
        assert_eq!(model_choices_static(&cfg, Some("b"), Some("m2")), rows);
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
