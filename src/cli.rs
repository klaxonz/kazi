//! CLI surface (clap subcommands) and dispatch into the agent engine.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::agent::{context::History, repl, session, Agent, AgentError};
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

/// Dispatch; returns the process exit code.
pub async fn execute(cli: Cli) -> anyhow::Result<u8> {
    match cli.command {
        None => chat(&EngineArgs::default(), cli.config.as_deref()).await,
        Some(Command::Chat(args)) => chat(&args, cli.config.as_deref()).await,
        Some(Command::Run { task, args }) => run_once(&task, &args, cli.config.as_deref()).await,
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

async fn run_once(task: &str, args: &EngineArgs, cfg_path: Option<&Path>) -> anyhow::Result<u8> {
    let (cfg, _) = config::load(cfg_path)?;
    let agent = Agent::default_cli(
        &cfg,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.yolo,
    )?;
    let mut agent = if let Some(name) = &args.resume {
        match session::load(name) {
            Ok(f) => {
                let sys = f.system.or_else(|| agent.history.system.clone());
                let mut a = agent;
                a.history = History::new(sys);
                a.history.msgs = f.messages;
                println!(
                    "{DIM}resumed `{name}` ({n} messages){RESET}",
                    n = a.history.msgs_len()
                );
                a
            }
            Err(e) => {
                println!("{YELLOW}cannot resume `{name}`: {e}{RESET}");
                agent
            }
        }
    } else {
        agent
    };

    println!("{DIM}provider: {}{RESET}", agent.provider().describe());
    match agent.run_task(task, |e| repl::render_event(&e)).await {
        Ok(_answer) => {
            println!();
            Ok(0)
        }
        Err(AgentError::RoundsExceeded(r)) => {
            eprintln!("\n{YELLOW}stopped after {r} rounds without a final answer{RESET}");
            Ok(2)
        }
        Err(e) => {
            eprintln!("\n{RED}{e}{RESET}");
            Ok(1)
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
