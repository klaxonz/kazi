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
use crate::tools::{Approver, Decision};

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Undo the last task's file changes via the agent's checkpoint store.
/// Shared by the REPL and the TUI.
pub fn undo_last_task(agent: &Agent) -> std::io::Result<Option<usize>> {
    match &agent.ctx.checkpointing {
        Some(cp) => cp.undo_latest(),
        None => Ok(None),
    }
}

/// `/undo` — text both front-ends print.
pub fn undo_text(agent: &Agent) -> String {
    match undo_last_task(agent) {
        Ok(Some(n)) => format!("restored {n} file(s) from the last task's checkpoint"),
        Ok(None) => "nothing to undo (no checkpointed task yet)".to_string(),
        Err(e) => format!("undo failed: {e}"),
    }
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

/// `/resume` with no argument: what is stored.
pub fn sessions_text() -> String {
    let have = session::list();
    if have.is_empty() {
        "no stored sessions yet — sessions save automatically after each task".to_string()
    } else {
        format!(
            "stored sessions: {}\n{}",
            have.join(", "),
            usage_for("/resume")
        )
    }
}

/// `/resume <prefix>`: load a stored session into the agent.
pub async fn resume_text(agent: &mut Agent, prefix: &str) -> String {
    match session::resolve(prefix) {
        Some(name) => match super::session::load(&name) {
            Ok(file) => {
                let sys = file.system.or_else(|| agent.history.system.clone());
                agent.history = History::new(sys);
                agent.history.msgs = file.messages;
                let count = agent.history.msgs_len();
                format!("loaded {name} ({count} messages)")
            }
            Err(e) => format!("resume failed: {e}"),
        },
        None => {
            let have = session::list().join(", ");
            format!("no stored session matches `{prefix}` (stored: {have})")
        }
    }
}

/// `/compact` — squeeze the dropped turns into a system-prompt summary.
pub async fn compact_text(agent: &mut Agent) -> String {
    match agent.compact().await {
        Ok(note) => format!("{note} (est. {} tokens)", agent.history.estimate_tokens()),
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

/// `/provider` with no argument: what is configured.
pub fn providers_text(cfg: &Config) -> String {
    format!(
        "configured providers: {}\n{}",
        cfg.nicks().join(", "),
        usage_for("/provider")
    )
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

/// `/context`: where the estimated tokens go right now.
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
    format!(
        "context estimate:\n  system   ~{sys} tokens\n  history  ~{hist} tokens ({} messages)\n  tools    ~{tools} tokens ({} in catalog)\n  total    ~{} / {} token budget",
        h.msgs_len(),
        specs.len(),
        sys + hist + tools,
        agent.max_context_tokens()
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
        _ => Decision::Deny,
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
            "\n{YELLOW}[{tool}] requests approval{RESET}\n{summary}\napprove? [y] once  [a] always (session)  [N] no "
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
        args: "",
        help: "drop oldest turns under the context budget",
        arg: ArgKind::No,
    },
    CmdSpec {
        name: "/resume",
        args: "<prefix>",
        help: "load a stored session (TAB lists them)",
        arg: ArgKind::Session,
    },
    CmdSpec {
        name: "/provider",
        args: "<nick>",
        help: "switch provider (TAB lists them)",
        arg: ArgKind::Provider,
    },
    CmdSpec {
        name: "/model",
        args: "<name>",
        help: "override the current provider's model",
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
        arg: ArgKind::No,
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
        help: "provider, model, rounds, usage, session",
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
    yolo: bool,
}

impl Completer for SlashHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> RlResult<(usize, Vec<Pair>)> {
        Ok(self.completions(line, pos).unwrap_or((pos, Vec::new())))
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
            ArgKind::Toggle => ["on", "off"]
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
    mode: crate::tools::PermissionMode,
    session_name: String,
}

impl Clone for ChatState {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
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
        Self {
            cfg,
            provider,
            model,
            mode,
            session_name,
        }
    }

    /// (/new, /clear) points future auto-saves at a different session file.
    pub fn session_name_mut(&mut self) -> &mut String {
        &mut self.session_name
    }

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
            None => println!("{YELLOW}no stored session matches `{name}`{RESET}"),
        }
    }
    let banner_provider = live.describe();
    println!(
        "{CYAN}kazi{RESET} v{} — {banner_provider}{DIM}\nsession: {}{RESET}\n{DIM}{}{RESET}",
        env!("CARGO_PKG_VERSION"),
        state.session_name,
        help_text().trim_end()
    );

    let rl_cfg = RlConfig::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut rl: Editor<SlashHelper, DefaultHistory> = Editor::with_config(rl_cfg)?;
    rl.set_helper(Some(SlashHelper {
        nicks: state.cfg.nicks(),
        model: state.model.clone(),
        yolo: state.mode == crate::tools::PermissionMode::Yolo,
    }));

    loop {
        let line = match rl.readline("kazi> ") {
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

        if line.starts_with('/') {
            // pass the full line (with '/') — match arms key on the slash form
            if handle_command(&line, &mut state, &mut live).await.is_exit() {
                break;
            }
            // keep completion hints honest with whatever the command changed
            if let Some(h) = rl.helper_mut() {
                h.yolo = state.mode == crate::tools::PermissionMode::Yolo;
                h.model = state.model.clone();
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
                println!("\n{RED}{e}{RESET}");
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
    let (verb, arg) = cmd
        .split_once(' ')
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((cmd, ""));

    match (verb, arg) {
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
        ("/compact", _) => println!("{}", compact_text(&mut live.agent).await),
        ("/status", _) => println!("{}", status_text(&live.agent)),
        ("/tools", _) => println!("{}", live.agent.registry().catalog()),
        ("/resume", prefix) => {
            if prefix.is_empty() {
                println!("{}", sessions_text());
            } else {
                println!("{}", resume_text(&mut live.agent, prefix).await);
            }
        }
        ("/provider", nick) => {
            if nick.is_empty() {
                println!("{}", providers_text(&state.cfg));
            } else {
                let mut next = state.clone();
                next.provider = Some(nick.to_string());
                match next.rebuild().await {
                    Ok(new) => {
                        println!("switched -> {}", new.describe());
                        *live = new;
                    }
                    Err(e) => println!("{RED}switch failed: {e}{RESET}"),
                }
            }
        }
        ("/model", m) => {
            if m.is_empty() {
                println!("current: {}\n{}", live.describe(), usage_for("/model"));
            } else {
                let mut next = state.clone();
                next.model = Some(m.to_string());
                match next.rebuild().await {
                    Ok(mut new) => {
                        // Dry-run before committing: a wrong model name only
                        // fails at the next API call otherwise (HTTP 4xx with
                        // context now gone). One tiny probe round verifies the
                        // model exists on this provider; failure keeps the
                        // current live agent untouched.
                        match new.agent.probe().await {
                            Ok(()) => {
                                println!("switched -> {}", new.describe());
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
            let prev = state.mode;
            state.mode = next;
            match state.rebuild().await {
                Ok(new) => {
                    *live = new;
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
                Err(e) => {
                    state.mode = prev; // roll back on failed rebuild
                    println!("{RED}toggle failed: {e}{RESET}");
                }
            }
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
                    let prev = state.mode;
                    state.mode = m;
                    match state.rebuild().await {
                        Ok(new) => {
                            *live = new;
                            println!("permission mode -> {}", state.mode.as_str());
                        }
                        Err(e) => {
                            state.mode = prev;
                            println!("{RED}switch failed: {e}{RESET}");
                        }
                    }
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
        // toggle offers on/off
        let (_, cands) = helper.completions("/yolo o", 7).expect("in scope");
        assert_eq!(cands.len(), 2);
        // non-slash line: out of scope
        assert!(helper.completions("list files", 10).is_none());
        // unknown arg kind → empty candidates, still in scope
        let (_, cands) = helper.completions("/model m", 8).expect("in scope");
        assert!(cands.is_empty());
    }

    #[test]
    fn hints_show_argument_picture_and_options() {
        let helper = SlashHelper {
            nicks: vec!["a".into()],
            model: Some("m1".into()),
            yolo: true,
        };
        let history = DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        let hint = helper.hint("/model", 6, &ctx).expect("hint");
        assert!(hint.contains("<name>") && hint.contains("m1"));
        let hint = helper
            .hint("/provider ", 10, &ctx)
            .expect("hint after space");
        assert!(hint.contains("one of: a"));
        // mid-line cursor or past the verb → silent
        assert!(helper.hint("/model gpt", 10, &ctx).is_none());
        assert!(helper.hint("plain text", 10, &ctx).is_none());
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
        assert_eq!(decision_from_input("n\n"), Decision::Deny);
        assert_eq!(decision_from_input(""), Decision::Deny);
        assert_eq!(decision_from_input("what?\n"), Decision::Deny);
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
