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
  /clear              forget the conversation, keep the system prompt
  /compact            drop oldest turns under the context budget
  /save [name]        save the session under a name
  /resume <name>      load a stored session
  /provider <nick>    switch provider (see `kazi providers`)
  /model <name>       override the current provider's model
  /tools              list built-in tools
  /status             provider, model, rounds, estimated context";

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

    fn rebuild(&self) -> anyhow::Result<LiveAgent> {
        Agent::default_cli(
            &self.cfg,
            self.provider.as_deref(),
            self.model.as_deref(),
            self.yolo,
        )
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
        format!(
            "provider/model: {}\nround budget:   {}\nest. context:   {} tokens\nsession: {}",
            self.describe(),
            self.agent.max_rounds(),
            self.agent.history.estimate_tokens(),
            self.agent.history.msgs_len()
        )
    }
    fn history(&self) -> &History {
        &self.agent.history
    }
}

/// Run the interactive loop; returns the process exit code.
pub async fn chat_loop(
    cfg: Config,
    provider: Option<String>,
    model: Option<String>,
    yolo: bool,
    resume: Option<String>,
) -> anyhow::Result<u8> {
    let session_name = resume.clone().unwrap_or_else(session::default_name);
    let mut state = ChatState::new(cfg, provider, model, yolo, session_name);
    let mut live = state.rebuild()?;

    if let Some(name) = &resume {
        match session::load(name) {
            Ok(f) => {
                let n = f.messages.len();
                let sys = f.system.or_else(|| live.history().system.clone());
                live.agent.history = History::new(sys);
                live.agent.history.msgs = f.messages;
                println!("{DIM}resumed `{name}` ({n} messages){RESET}");
            }
            Err(e) => println!("{YELLOW}cannot resume `{name}`: {e}{RESET}"),
        }
    }
    println!(
        "{CYAN}kazi{RESET} v{} — {}\n{DIM}{HELP}{RESET}",
        env!("CARGO_PKG_VERSION"),
        live.describe()
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

        if let Some(cmd) = line.strip_prefix('/') {
            if handle_command(cmd, &mut state, &mut live).is_exit() {
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

fn handle_command(cmd: &str, state: &mut ChatState, live: &mut LiveAgent) -> Flow {
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
            println!("history cleared");
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
        ("/resume", name) if !name.is_empty() => match session::load(name) {
            Ok(f) => {
                let n = f.messages.len();
                let sys = f.system.or_else(|| live.history().system.clone());
                live.agent.history = History::new(sys);
                live.agent.history.msgs = f.messages;
                println!("loaded {name} ({n} messages)");
            }
            Err(e) => println!("{RED}resume failed: {e}{RESET}"),
        },
        ("/provider", nick) if !nick.is_empty() => {
            let mut next = state.clone();
            next.provider = Some(nick.to_string());
            match next.rebuild() {
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
            match next.rebuild() {
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
