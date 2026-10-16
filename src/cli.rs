//! CLI surface (clap subcommands) and dispatch into the agent engine.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::agent::{context::History, repl, session, Agent, AgentError, AgentEvent};
use crate::config;
use crate::tools::ToolRegistry;

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

#[derive(Parser, Debug)]
#[command(
    name = "kazi",
    version,
    about = "A tiny, from-scratch AI agent for your terminal",
    after_help = "Providers are configured in config.toml — see `kazi config`."
)]
pub struct Cli {
    /// Path to the config file (default: platform config dir)
    #[arg(long, global = true, help_heading = "Config")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags shared by `chat` and `run`.
#[derive(Args, Debug, Default)]
pub struct EngineArgs {
    /// Provider nickname from config (deepseek, anthropic, ollama, ...)
    #[arg(long)]
    pub provider: Option<String>,
    /// Model override for the selected provider
    #[arg(long)]
    pub model: Option<String>,
    /// Load a previously saved session by name
    #[arg(long)]
    pub resume: Option<String>,
    /// Auto-approve run_command without asking (dangerous)
    #[arg(long)]
    pub yolo: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Interactive chat session (the default when no subcommand is given)
    #[command(name = "chat")]
    Chat(EngineArgs),
    /// Run one task, stream progress, print the final answer, exit 0/1/2
    #[command(name = "run")]
    Run {
        task: String,
        #[command(flatten)]
        args: EngineArgs,
        /// Structured stdout for scripts/CI (`json`); default: human text
        #[arg(long, value_enum)]
        output: Option<OutputFmt>,
    },
    /// Show the config path; `--init` writes the example config
    #[command(name = "config")]
    Config {
        #[arg(long = "init")]
        init: bool,
    },
    /// List configured providers
    #[command(name = "providers")]
    Providers,
    /// List built-in tools
    #[command(name = "tools")]
    Tools,
}

/// Machine-readable output shapes for `kazi run`.
#[derive(ValueEnum, Clone, Debug, Copy, PartialEq)]
pub enum OutputFmt {
    /// Stream progress, print the final answer (default).
    Text,
    /// One JSON object on stdout; events embedded; nothing else printed.
    Json,
}

/// Dispatch; returns the process exit code.
pub async fn execute(cli: Cli) -> anyhow::Result<u8> {
    match cli.command {
        None => chat(&EngineArgs::default(), cli.config.as_deref()).await,
        Some(Command::Chat(args)) => chat(&args, cli.config.as_deref()).await,
        Some(Command::Run { task, args, output }) => {
            run_once(
                &task,
                &args,
                output.unwrap_or(OutputFmt::Text),
                cli.config.as_deref(),
            )
            .await
        }
        Some(Command::Config { init }) => config_cmd(init),
        Some(Command::Providers) => providers_cmd(cli.config.as_deref()),
        Some(Command::Tools) => tools_cmd(),
    }
}

async fn chat(args: &EngineArgs, cfg_path: Option<&Path>) -> anyhow::Result<u8> {
    let (cfg, _) = config::load(cfg_path)?;
    repl::chat_loop(
        cfg,
        args.provider.clone(),
        args.model.clone(),
        args.yolo,
        args.resume.clone(),
    )
    .await
}

#[derive(Serialize)]
struct RunReport {
    ok: bool,
    provider: String,
    task: String,
    answer: String,
    /// `text_delta` / `tool_start` / `tool_end` events, in order.
    events: Vec<AgentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn run_once(
    task: &str,
    args: &EngineArgs,
    fmt: OutputFmt,
    cfg_path: Option<&Path>,
) -> anyhow::Result<u8> {
    let (cfg, _) = config::load(cfg_path)?;
    let agent = Agent::default_cli(
        &cfg,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.yolo,
    )
    .await?;
    let describe = agent.provider().describe();
    let mut agent = if let Some(name) = &args.resume {
        match session::load(name) {
            Ok(f) => {
                let sys = f.system.or_else(|| agent.history.system.clone());
                let mut a = agent;
                a.history = History::new(sys);
                a.history.msgs = f.messages;
                if fmt == OutputFmt::Text {
                    println!("resumed `{name}` ({n} messages)", n = a.history.msgs_len());
                }
                a
            }
            Err(e) => {
                println!("cannot resume `{name}`: {e}");
                agent
            }
        }
    } else {
        agent
    };

    let mut events: Vec<AgentEvent> = Vec::new();
    let mut on_event: Box<dyn FnMut(AgentEvent)> = match fmt {
        OutputFmt::Text => Box::new(|e| repl::render_event(&e)),
        OutputFmt::Json => Box::new(|e| events.push(e)),
    };

    let result = agent.run_task(task, &mut on_event).await;
    // the event collector holds `events`; release it before assembling output
    drop(on_event);
    match result {
        Ok(answer) => {
            if fmt == OutputFmt::Json {
                let report = RunReport {
                    ok: true,
                    provider: describe.clone(),
                    task: task.to_string(),
                    answer,
                    events,
                    error: None,
                };
                println!("{}", serde_json::to_string(&report).unwrap_or_default());
            } else {
                println!();
            }
            Ok(0)
        }
        Err(e) => {
            let (code, held) = match &e {
                AgentError::RoundsExceeded(_) => (2u8, "task budget exhausted"),
                _ => (1u8, "agent failed"),
            };
            if fmt == OutputFmt::Json {
                let report = RunReport {
                    ok: false,
                    provider: describe.clone(),
                    task: task.to_string(),
                    answer: String::new(),
                    events,
                    error: Some(e.to_string()),
                };
                println!("{}", serde_json::to_string(&report).unwrap_or_default());
            } else {
                eprintln!("\n{YELLOW}{held}{RESET}\n{RED}{e}{RESET}");
            }
            Ok(code)
        }
    }
}

fn config_cmd(init: bool) -> anyhow::Result<u8> {
    let path = config::config_path();
    if init {
        match config::write_example(&path) {
            Ok(true) => println!("wrote example config -> {}", path.display()),
            Ok(false) => println!("config already exists at {}", path.display()),
            Err(e) => {
                println!("{RED}cannot write {path:?}: {e}{RESET}");
                return Ok(1);
            }
        }
        println!("edit provider/model there, then export its `api_key_env` variable.");
        return Ok(0);
    }
    println!("config path: {}", path.display());
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(t) => println!("\n{DIM}--- contents ---{RESET}\n{t}"),
            Err(e) => println!("{RED}cannot read {path:?}: {e}{RESET}"),
        }
    }
    Ok(0)
}

fn providers_cmd(cfg_path: Option<&Path>) -> anyhow::Result<u8> {
    let (cfg, path) = config::load(cfg_path)?;
    println!("{DIM}config: {}{RESET}", path.display());
    for (nick, entry) in &cfg.providers {
        let model = entry
            .model
            .as_deref()
            .unwrap_or("(no model set: pass --model)");
        println!(
            "{CYAN}{nick:<14}{RESET} type={} model={model}",
            entry.r#type
        );
    }
    Ok(0)
}

fn tools_cmd() -> anyhow::Result<u8> {
    println!("{}", ToolRegistry::builtin().catalog());
    Ok(0)
}
