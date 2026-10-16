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
use crate::tools::Approver;

const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Terminal prompt approver: shows the action, waits for y/N. A blocking
/// stdin read is deliberate here — nothing else runs while we wait.
pub struct StdinApprover;

#[async_trait]
impl Approver for StdinApprover {
    async fn decide(&self, tool: &str, summary: &str) -> bool {
        print!("\n{YELLOW}[{tool}] requests approval{RESET}\n{summary}\napprove? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
    CmdSpec { name: "/help",    args: "",          help: "show this help", arg: ArgKind::No },
    CmdSpec { name: "/exit",    args: "",          help: "save session and leave (Ctrl-D also quits)", arg: ArgKind::No },
    CmdSpec { name: "/quit",    args: "",          help: "alias for /exit", arg: ArgKind::No },
    CmdSpec { name: "/new",     args: "[name]",    help: "fresh session (optional name); the old one keeps on exit", arg: ArgKind::Topic },
    CmdSpec { name: "/clear",   args: "",          help: "forget history; autosave moves to a fresh session file", arg: ArgKind::No },
    CmdSpec { name: "/compact", args: "",          help: "drop oldest turns under the context budget", arg: ArgKind::No },
    CmdSpec { name: "/resume",  args: "<prefix>",  help: "load a stored session (TAB lists them)", arg: ArgKind::Session },
    CmdSpec { name: "/provider", args: "<nick>",   help: "switch provider (TAB lists them)", arg: ArgKind::Provider },
    CmdSpec { name: "/model",   args: "<name>",    help: "override the current provider's model", arg: ArgKind::Model },
    CmdSpec { name: "/yolo",    args: "[on|off]",  help: "toggle command auto-approval", arg: ArgKind::Toggle },
    CmdSpec { name: "/tools",   args: "",          help: "list built-in tools (+ MCP servers)", arg: ArgKind::No },
    CmdSpec { name: "/status",  args: "",          help: "provider, model, rounds, usage, session", arg: ArgKind::No },
];

fn spec_of(verb: &str) -> Option<&'static CmdSpec> {
    COMMANDS.iter().find(|c| c.name == verb)
}

/// Rendered command table (`/help` output and the startup banner share this).
fn help_text() -> String {
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
        Cow::Owned(format!(
            "{CYAN}{}{RESET}{}",
            &line[..end],
            &line[end..]
        ))
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
    yolo: bool,
    session_name: String,
}

impl Clone for ChatState {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            yolo: self.yolo,
            session_name: self.session_name.clone(),
        }
    }
}

impl ChatState {
    pub fn new(
        cfg: Config,
        provider: Option<String>,
        model: Option<String>,
        yolo: bool,
        session_name: String,
    ) -> Self {
        Self {
            cfg,
            provider,
            model,
            yolo,
            session_name,
        }
    }

    /// (/new, /clear) points future auto-saves at a different session file.
    pub fn session_name_mut(&mut self) -> &mut String {
        &mut self.session_name
    }

    async fn rebuild(&self) -> anyhow::Result<LiveAgent> {
        Agent::default_cli(
            &self.cfg,
            self.provider.as_deref(),
            self.model.as_deref(),
            self.yolo,
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
    fn status(&self) -> String {
        let u = self.agent.usage();
        format!(
            "provider/model: {}\nround budget:   {}\nest. context:   {} tokens\nusage(task):    in {} / out {} tokens\nmessages:       {}",
            self.describe(),
            self.agent.max_rounds(),
            self.agent.history.estimate_tokens(),
            u.input_tokens.unwrap_or(0),
            u.output_tokens.unwrap_or(0),
            self.agent.history.msgs_len()
        )
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
    yolo: bool,
    topic: Option<String>,
    resume: Option<String>,
) -> anyhow::Result<u8> {
    // explicit --resume wins; otherwise a fresh named session from the topic
    let session_name = match resume.as_deref() {
        Some(name) => session::resolve(name).unwrap_or_else(|| name.to_string()),
        None => pick_session_name(topic.as_deref()),
    };
    let mut state = ChatState::new(cfg, provider, model, yolo, session_name.clone());
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
        yolo: state.yolo,
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
        let _ = rl.add_history_entry(&line);

        if line.starts_with('/') {
            // pass the full line (with '/') — match arms key on the slash form
            if handle_command(&line, &mut state, &mut live).await.is_exit() {
                break;
            }
            // keep completion hints honest with whatever the command changed
            if let Some(h) = rl.helper_mut() {
                h.yolo = state.yolo;
                h.model = state.model.clone();
            }
            continue;
        }

        // A real task: stream it, then auto-save.
        println!();
        match live.agent.run_task(&line, |e| render_event(&e)).await {
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
        ("/exit", _) | ("/quit", _) => return Flow::Exit,
        ("/clear", _) => {
            // clear + repoint autosave: the old file can never be rewritten
            let name = pick_session_name(None);
            *state.session_name_mut() = name;
            live.agent.reset();
            println!("{GREEN}history cleared{RESET} — autosave now -> {}", state.session_name);
        }
        ("/new", topic) => {
            let name = pick_session_name(if topic.is_empty() { None } else { Some(topic) });
            *state.session_name_mut() = name;
            live.agent.reset();
            println!("new session -> {}", state.session_name);
        }
        ("/compact", _) => match live.agent.compact().await {
            Ok(note) => {
                println!(
                    "{note} (est. {} tokens)",
                    live.agent.history.estimate_tokens()
                );
            }
            Err(e) => println!("{RED}compact failed: {e}{RESET}"),
        },
        ("/status", _) => println!("{}", live.status()),
        ("/tools", _) => println!("{}", live.agent.registry().catalog()),
        ("/resume", prefix) => {
            if prefix.is_empty() {
                let have = session::list();
                if have.is_empty() {
                    println!("no stored sessions yet — sessions save automatically after each task");
                } else {
                    println!("stored sessions: {}\n{}", have.join(", "), usage_line(spec_of("/resume").unwrap()));
                }
            } else {
                match session::resolve(prefix) {
                    Some(n) => match load_session(live, &n) {
                        Ok(count) => println!("loaded {n} ({count} messages)"),
                        Err(e) => println!("{RED}resume failed: {e}{RESET}"),
                    },
                    None => {
                        let have = session::list().join(", ");
                        println!("{RED}no stored session matches `{prefix}`{RESET}{DIM} (stored: {have}){RESET}");
                    }
                }
            }
        }
        ("/provider", nick) => {
            if nick.is_empty() {
                println!(
                    "configured providers: {}\n{}",
                    state.cfg.nicks().join(", "),
                    usage_line(spec_of("/provider").unwrap())
                );
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
                println!(
                    "current: {}\n{}",
                    live.describe(),
                    usage_line(spec_of("/model").unwrap())
                );
            } else {
                let mut next = state.clone();
                next.model = Some(m.to_string());
                match next.rebuild().await {
                    Ok(new) => {
                        println!("switched -> {}", new.describe());
                        *live = new;
                    }
                    Err(e) => println!("{RED}switch failed: {e}{RESET}"),
                }
            }
        }
        ("/yolo", on) => {
            let next = match on {
                "" => !state.yolo,
                "on" | "1" | "true" => true,
                "off" | "0" | "false" => false,
                _ => {
                    println!("{YELLOW}{}{RESET}", usage_line(spec_of("/yolo").unwrap()));
                    return Flow::Continue;
                }
            };
            state.yolo = next;
            match state.rebuild().await {
                Ok(new) => {
                    *live = new;
                    println!(
                        "command auto-approval: {}",
                        if state.yolo {
                            format!("{GREEN}ON{RESET} (--yolo) — run_command executes without prompts")
                        } else {
                            format!("{YELLOW}OFF{RESET} — run_command asks for approval")
                        }
                    );
                }
                Err(e) => {
                    state.yolo = !next; // roll back on failed rebuild
                    println!("{RED}toggle failed: {e}{RESET}");
                }
            }
        }
        _ => {
            let near = did_you_mean(verb);
            let hint = if near.is_empty() {
                "see /help".to_string()
            } else {
                format!("did you mean {}?", near.join(", "))
            };
            println!("{YELLOW}unknown command `{verb}`{RESET}{DIM} — {hint}{RESET}");
        }
    }
    Flow::Continue
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (start, cands) = helper
            .completions("/provider de", 12)
            .expect("in scope");
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
        let hint = helper.hint("/provider ", 10, &ctx).expect("hint after space");
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
        std::fs::remove_file(session::sessions_dir().unwrap().join(format!("{base}.json")))
            .unwrap();
    }
}

#[cfg(test)]
mod slug_tests {
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
