//! Interactive chat (`kazi chat`): streaming output, slash commands, session
//! auto-save, live provider/model switching.
//!
//! Printing is ASCII (+ VT colors where harmless) so output stays readable
//! even on legacy Windows codepages (GBK/CMD).

use std::io::Write;

use async_trait::async_trait;
use rustyline::error::ReadlineError;

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

const HELP: &str = "\
commands:
  /help               show this help
  /exit | /quit       save session and leave (Ctrl-D also quits)
  /new [topic]        start a fresh named session (keep the current one)
  /clear              forget the conversation, keep the session file
  /compact            drop oldest turns under the context budget
  /save [name]        save the session under a name
  /resume <name>      load a stored session (prefix works)
  /provider <nick>    switch provider (see `kazi providers`)
  /model <name>       override the current provider's model
  /tools              list built-in tools (+ MCP servers)
  /status             provider, model, rounds, usage, session";

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

    /// (/new) points future auto-saves at a different session file.
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

/// Resolve the session name for a fresh chat: explicit topic slug, or the
/// timestamp default when the topic has nothing usable.
fn fresh_session_name(topic: Option<&str>) -> String {
    let base = match topic.map(slugify).filter(|s| !s.is_empty()) {
        Some(n) => n,
        None => return session::default_name(),
    };
    // same policy as /new: never clobber an existing session file
    let mut name = base.clone();
    let mut n = 2;
    while session::load(&name).is_ok() {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
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
        None => fresh_session_name(topic.as_deref()),
    };
    let mut state = ChatState::new(cfg, provider, model, yolo, session_name.clone());
    let mut live = state.rebuild().await?;

    if let Some(name) = &resume {
        match session::resolve(name) {
            Some(n) => match session::load(&n) {
                Ok(f) => {
                    let count = f.messages.len();
                    let sys = f.system.or_else(|| live.history().system.clone());
                    live.agent.history = History::new(sys);
                    live.agent.history.msgs = f.messages;
                    println!("{DIM}resumed `{n}` ({count} messages){RESET}");
                }
                Err(e) => println!("{YELLOW}cannot resume `{n}`: {e}{RESET}"),
            },
            None => println!("{YELLOW}no stored session matches `{name}`{RESET}"),
        }
    }
    println!(
        "{CYAN}kazi{RESET} v{} — {}{DIM}\nsession: {}{RESET}\n{DIM}{HELP}{RESET}",
        env!("CARGO_PKG_VERSION"),
        live.describe(),
        state.session_name
    );

    let mut rl = rustyline::DefaultEditor::new()?;

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
            // pass the full line (with '/') — arms match on the slash form
            if handle_command(&line, &mut state, &mut live).await.is_exit() {
                break;
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

    // empty sessions (no messages) never touch the disk — /save overrides
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

    fn save_session(live: &LiveAgent, name: &str) -> bool {
        match session::save(name, live.history().system.as_deref(), &live.history().msgs) {
            Ok(p) => {
                println!("saved -> {}", p.display());
                true
            }
            Err(e) => {
                println!("{RED}save failed: {e}{RESET}");
                false
            }
        }
    }

    match (verb, arg) {
        ("/help", _) => println!("{HELP}"),
        ("/exit", _) | ("/quit", _) => return Flow::Exit,
        ("/clear", _) => {
            live.agent.reset();
            println!("history cleared (same session)");
        }
        ("/new", topic) => {
            let mut name = slugify(topic);
            if name.is_empty() {
                name = session::default_name();
            }
            // colliding stem: attach a suffix rather than clobbering history
            let mut n = 2;
            while session::load(&name).is_ok() {
                name = format!("{name}-{n}");
                n += 1;
            }
            *state.session_name_mut() = name;
            live.agent.reset();
            println!("new session -> {}", state.session_name);
        }
        ("/compact", _) => {
            live.agent.compact();
            println!(
                "compacted (est. {} tokens)",
                live.agent.history.estimate_tokens()
            );
        }
        ("/status", _) => println!("{}", live.status()),
        ("/tools", _) => println!("{}", live.agent.registry().catalog()),
        ("/save", name) => {
            let name = if name.is_empty() {
                state.session_name.clone()
            } else {
                name.to_string()
            };
            if save_session(live, &name) {
                state.session_name = name;
            }
        }
        ("/resume", prefix) if !prefix.is_empty() => match session::resolve(prefix) {
            Some(n) => match session::load(&n) {
                Ok(f) => {
                    let count = f.messages.len();
                    let sys = f.system.or_else(|| live.history().system.clone());
                    live.agent.history = History::new(sys);
                    live.agent.history.msgs = f.messages;
                    println!("loaded {n} ({count} messages)");
                }
                Err(e) => println!("{RED}resume failed: {e}{RESET}"),
            },
            None => println!("{RED}no stored session matches `{prefix}`{RESET}"),
        },
        ("/provider", nick) if !nick.is_empty() => {
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
        ("/model", m) if !m.is_empty() => {
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
        _ => println!("{YELLOW}unknown command — /help{RESET}"),
    }
    Flow::Continue
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
