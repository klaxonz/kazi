//! CLI surface (clap subcommands) and dispatch into the agent engine.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::agent::{context::History, repl, session, tui, Agent, AgentError, AgentEvent};
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
    after_help = "Providers are configured in config.toml — see `kazi config`.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Path to the config file (default: platform config dir)
    #[arg(long, global = true, help_heading = "Config")]
    pub config: Option<PathBuf>,
    /// Flags for the bare invocation (`kazi` == `kazi chat`), so `kazi --yolo`
    /// works without naming the subcommand.
    #[command(flatten)]
    pub engine: EngineArgs,
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
    /// Startup permission mode: read-only | edit (default) | yolo
    #[arg(long = "permission-mode", value_name = "MODE")]
    pub permission_mode: Option<String>,
    /// Force the classic line REPL instead of the TUI (pipes/scripts always
    /// get the line REPL automatically)
    #[arg(long = "no-tui")]
    pub no_tui: bool,
    /// Force the full-screen TUI even if autodetection would pick the line
    /// REPL (falls back gracefully if the terminal can't do alternate
    /// screens)
    #[arg(long, conflicts_with = "no_tui")]
    pub tui: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Interactive chat session (the default when no subcommand is given)
    #[command(name = "chat")]
    Chat {
        /// Topic to name the new session after (slugified; bare = timestamp)
        topic: Option<String>,
        #[command(flatten)]
        args: EngineArgs,
    },
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
    /// Manage locally stored API keys (`<data-dir>/kazi/auth.json`)
    #[command(name = "auth")]
    Auth {
        #[command(subcommand)]
        cmd: Option<AuthCmd>,
    },
    /// List built-in tools
    #[command(name = "tools")]
    Tools,
    /// List saved sessions
    #[command(name = "sessions")]
    Sessions,
    /// Delete a saved session by name (exact or unambiguous prefix)
    #[command(name = "rm")]
    Rm {
        /// Session name (exact or unambiguous prefix)
        session: String,
    },
    /// Export a saved session as markdown
    #[command(name = "export")]
    Export {
        /// Session name (exact or unambiguous prefix)
        session: String,
        /// Output path (default: `<session>.md` in the working directory)
        out: Option<PathBuf>,
    },
}

/// Manage locally stored API keys (environment variables always win).
#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Store the key for a provider (reads the value from an argument,
    /// stdin, or — interactively — a hidden prompt)
    Set {
        /// Provider nickname from config (see `kazi providers`)
        provider: String,
        /// The key itself; omit to paste it (hidden prompt or stdin pipe)
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
    /// Show which providers have a stored key (values are never printed)
    List,
    /// Remove the stored key for a provider
    Remove {
        /// Provider nickname (exact or unambiguous prefix)
        provider: String,
    },
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
        None => chat(&cli.engine, None, cli.config.as_deref()).await,
        Some(Command::Chat { topic, args }) => chat(&args, topic, cli.config.as_deref()).await,
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
        Some(Command::Auth { cmd }) => auth_cmd(cmd, cli.config.as_deref()),
        Some(Command::Tools) => tools_cmd(),
        Some(Command::Sessions) => sessions_cmd(),
        Some(Command::Rm { session }) => rm_cmd(&session),
        Some(Command::Export { session, out }) => export_cmd(&session, out.as_deref()),
    }
}

async fn chat(
    args: &EngineArgs,
    topic: Option<String>,
    cfg_path: Option<&Path>,
) -> anyhow::Result<u8> {
    let (cfg, _) = config::load(cfg_path)?;
    // TUI is the default on real terminals; pipes/redirects (scripts, tests)
    // always get the line REPL unless --tui is forced.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let want_tui = !args.no_tui && (args.tui || interactive);
    // On a real terminal a missing key should not hard-exit: offer to store
    // one (like claude/codex onboarding). Non-interactive paths keep the
    // fail-fast contract.
    let nick = match cfg.resolve(args.provider.as_deref(), args.model.as_deref()) {
        Ok(r) => r.nick,
        Err(e) => anyhow::bail!("{e}"),
    };
    if let Err(e) = cfg.api_key_for(&nick) {
        if !interactive {
            return Err(e.into());
        }
        onboarding_offer(&cfg, &nick, e)?;
    }
    let mode = startup_mode(&cfg, args);
    if want_tui {
        return tui::chat_loop_with(
            cfg,
            args.provider.clone(),
            args.model.clone(),
            mode,
            topic,
            args.resume.clone(),
        )
        .await;
    }
    repl::chat_loop(
        cfg,
        args.provider.clone(),
        args.model.clone(),
        mode,
        topic,
        args.resume.clone(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_invocation_accepts_engine_flags() {
        // the reported gap: `kazi --yolo` must work without a subcommand
        let cli = Cli::try_parse_from(["kazi", "--yolo"]).expect("--yolo parses bare");
        assert!(cli.command.is_none());
        assert!(cli.engine.yolo);

        let cli = Cli::try_parse_from(["kazi", "--permission-mode", "read-only"]).unwrap();
        assert_eq!(cli.engine.permission_mode.as_deref(), Some("read-only"));

        let cli = Cli::try_parse_from(["kazi", "--model", "x", "--no-tui"]).unwrap();
        assert_eq!(cli.engine.model.as_deref(), Some("x"));
        assert!(cli.engine.no_tui);

        // the subcommand form keeps working
        let cli = Cli::try_parse_from(["kazi", "chat", "--yolo"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Chat { args, .. }) if args.yolo));

        // mixing bare flags WITH a subcommand is an error, never ignored
        assert!(Cli::try_parse_from(["kazi", "--yolo", "chat"]).is_err());
    }

    #[test]
    fn startup_mode_precedence() {
        use crate::tools::PermissionMode as Mode;
        let cfg = crate::config::Config::default();

        let mut args = EngineArgs::default();
        assert_eq!(startup_mode(&cfg, &args), Mode::Edit, "default");

        args.yolo = true;
        assert_eq!(
            startup_mode(&cfg, &args),
            Mode::Yolo,
            "--yolo wins over config"
        );

        args.permission_mode = Some("read-only".to_string());
        assert_eq!(
            startup_mode(&cfg, &args),
            Mode::ReadOnly,
            "--permission-mode wins over --yolo"
        );
    }
}

/// Startup permission mode: `--permission-mode` wins, then `--yolo`, then
/// `agent.permission_mode`, then the `edit` default.
fn startup_mode(cfg: &config::Config, args: &EngineArgs) -> crate::tools::PermissionMode {
    if let Some(m) = args
        .permission_mode
        .as_deref()
        .and_then(crate::tools::PermissionMode::parse)
    {
        return m;
    }
    Agent::initial_mode(cfg, args.yolo)
}

#[derive(Serialize)]
struct RunReport {
    ok: bool,
    provider: String,
    task: String,
    answer: String,
    /// `text_delta` / `tool_start` / `tool_end` events, in order.
    events: Vec<AgentEvent>,
    /// Provider-reported token usage of this task (zeros if unknown).
    usage: crate::llm::Usage,
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
    let mode = startup_mode(&cfg, args);
    // `run` is the scripting/CI path: fail fast with an actionable message
    // instead of agent-construction warnings + a mid-stream 401.
    let nick = cfg
        .resolve(args.provider.as_deref(), args.model.as_deref())?
        .nick;
    cfg.api_key_for(&nick)?;
    let agent =
        Agent::default_cli_mode(&cfg, args.provider.as_deref(), args.model.as_deref(), mode)
            .await?;
    let describe = agent.provider().describe();
    let mut agent = if let Some(raw) = &args.resume {
        match session::resolve(raw) {
            Some(name) => match session::load(&name) {
                Ok(f) => {
                    let sys = f.system.or_else(|| agent.history.system.clone());
                    let mut a = agent;
                    a.history = History::new(sys);
                    a.history.msgs = f.messages;
                    if fmt == OutputFmt::Text {
                        println!(
                            "resumed `{n}` ({count} messages)",
                            n = name,
                            count = a.history.msgs_len()
                        );
                    }
                    a
                }
                Err(e) => {
                    println!("cannot resume `{name}`: {e}");
                    agent
                }
            },
            None => {
                println!("no stored session matches `{raw}`");
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
    let usage = agent.usage();
    match result {
        Ok(answer) => {
            if fmt == OutputFmt::Json {
                let report = RunReport {
                    ok: true,
                    provider: describe.clone(),
                    task: task.to_string(),
                    answer,
                    events,
                    usage,
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
                    usage,
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
    let store = crate::auth::load().ok();
    for (nick, entry) in &cfg.providers {
        let model = entry
            .model
            .as_deref()
            .unwrap_or("(no model set: pass --model)");
        let key = match cfg.kind_of_nick(nick) {
            Ok(crate::llm::ProviderKind::Ollama) => "no key needed".to_string(),
            Ok(kind) => {
                let env_name = entry
                    .api_key_env
                    .clone()
                    .or_else(|| {
                        config::Config::default_key_env(kind).map(str::to_string)
                    })
                    .unwrap_or_default();
                let env_ok = std::env::var(&env_name)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false);
                let stored = store
                    .as_ref()
                    .map(|s| s.keys.contains_key(nick))
                    .unwrap_or(false);
                if env_ok {
                    format!("{DIM}key: {env_name} ✓{RESET}")
                } else if stored {
                    format!("{DIM}key: stored (kazi auth){RESET}")
                } else {
                    format!("{YELLOW}key: missing — export {env_name} or `kazi auth set {nick}`{RESET}")
                }
            }
            Err(_) => String::new(),
        };
        println!(
            "{CYAN}{nick:<14}{RESET} type={} model={model}  {key}",
            entry.r#type
        );
    }
    Ok(0)
}

/// `kazi auth`: manage locally stored API keys. Values are never echoed back.
fn auth_cmd(cmd: Option<AuthCmd>, cfg_path: Option<&Path>) -> anyhow::Result<u8> {
    let cmd = cmd.unwrap_or(AuthCmd::List);
    match cmd {
        AuthCmd::List => {
            let store = crate::auth::load()?;
            if store.keys.is_empty() {
                println!("no stored keys yet — add one with: {CYAN}kazi auth set <provider>{RESET}");
                return Ok(0);
            }
            let path = crate::auth::auth_path()?;
            println!("{DIM}stored keys (env vars still win): {}{RESET}", path.display());
            for nick in store.keys.keys() {
                println!("  {CYAN}{nick}{RESET}");
            }
            Ok(0)
        }
        AuthCmd::Set { provider, key } => {
            let (cfg, _) = config::load(cfg_path)?;
            let nick = resolve_provider_nick(&cfg, &provider)?;
            let kind = cfg
                .kind_of_nick(&nick)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if kind == crate::llm::ProviderKind::Ollama {
                println!("`{nick}` is an ollama provider — it needs no API key");
                return Ok(0);
            }
            let value = match key {
                Some(k) => k,
                None => read_secret()?,
            };
            let value = value.trim().to_string();
            if value.is_empty() {
                println!("{RED}empty key — nothing stored{RESET}");
                return Ok(1);
            }
            let path = crate::auth::store(&nick, &value)?;
            println!("stored key for {CYAN}{nick}{RESET} -> {}", path.display());
            println!("{DIM}env vars still take precedence when both are set{RESET}");
            Ok(0)
        }
        AuthCmd::Remove { provider } => {
            let (cfg, _) = config::load(cfg_path)?;
            let nick = resolve_provider_nick(&cfg, &provider)?;
            match crate::auth::remove(&nick) {
                Ok(true) => {
                    println!("removed stored key for {CYAN}{nick}{RESET}");
                    Ok(0)
                }
                Ok(false) => {
                    println!("no stored key for `{nick}`");
                    Ok(1)
                }
                Err(e) => {
                    println!("{RED}cannot remove: {e}{RESET}");
                    Ok(1)
                }
            }
        }
    }
}

/// Providers get prefix resolution like sessions; exact wins.
fn resolve_provider_nick(cfg: &config::Config, name: &str) -> anyhow::Result<String> {
    if cfg.providers.contains_key(name) {
        return Ok(name.to_string());
    }
    let matches: Vec<String> = cfg
        .nicks()
        .into_iter()
        .filter(|n| n.starts_with(name))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        0 => anyhow::bail!(
            "unknown provider `{name}` — configured are: {}",
            cfg.nicks().join(", ")
        ),
        _ => anyhow::bail!(
            "ambiguous provider prefix `{name}` — matches: {}",
            matches.join(", ")
        ),
    }
}

/// Read a secret without echoing: hidden prompt on a terminal, plain read
/// from piped stdin (scripts), trimmed.
fn read_secret() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        // hidden prompt without pulling in a password crate:termios on unix,
        // console mode on windows
        print!("paste key (hidden), Enter to confirm: ");
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut buf = String::new();
        read_line_hidden(&mut buf)?;
        println!();
        Ok(buf)
    } else {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok(buf)
    }
}

/// One line of input with terminal echo disabled, platform-appropriate.
fn read_line_hidden(out: &mut String) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // least-bad portable option: read via console mode toggle
        use windows_hidden::*;
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        unsafe { GetConsoleMode(handle, &mut mode) };
        unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) };
        let reader = std::io::stdin();
        reader.read_line(out)?;
        unsafe { SetConsoleMode(handle, mode) };
        Ok(())
    }
    #[cfg(not(windows))]
    {
        // termios echo toggle, no external crate
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut t: libc_termios = unsafe { std::mem::zeroed() };
        unsafe { tcgetattr(fd, &mut t) };
        let saved = t;
        t.c_lflag &= !ECHO;
        unsafe { tcsetattr(fd, TCSANOW, &t) };
        let r = std::io::stdin().read_line(out);
        unsafe { tcsetattr(fd, TCSANOW, &saved) };
        r?;
        Ok(())
    }
}

#[cfg(windows)]
mod windows_hidden {
    // MINIMAL Win32 console API surface (no windows crate dependency)
    pub const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6u32 as i32 as u32;
    pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetStdHandle(n: u32) -> isize;
        pub fn GetConsoleMode(h: isize, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: isize, mode: u32) -> i32;
    }
}

#[cfg(not(windows))]
mod libc_termios_shim {
    // termios via libc types re-declared locally to avoid a dependency
    #[repr(C)]
    pub struct Termios {
        pub c_iflag: u32,
        pub c_oflag: u32,
        pub c_cflag: u32,
        pub c_lflag: u32,
        pub c_line: u8,
        pub c_cc: [u8; 32],
        pub c_ispeed: u32,
        pub c_ospeed: u32,
    }
    extern "C" {
        pub fn tcgetattr(fd: i32, t: *mut Termios) -> i32;
        pub fn tcsetattr(fd: i32, act: i32, t: *const Termios) -> i32;
    }
}
#[cfg(not(windows))]
use libc_termios_shim::*;
#[cfg(not(windows))]
const ECHO: u32 = 0o000010;
#[cfg(not(windows))]
const TCSANOW: i32 = 0;

/// Interactive offer when the default provider has no key: store it now
/// (recommended), export instructions, or just proceed (the session will
/// fail on first request). Never shown for ollama (needs no key).
fn onboarding_offer(
    cfg: &config::Config,
    nick: &str,
    err: config::ConfigError,
) -> anyhow::Result<()> {
    println!("{YELLOW}warning: {err}{RESET}");
    let needs_key = cfg
        .kind_of_nick(nick)
        .map(|k| k != crate::llm::ProviderKind::Ollama)
        .unwrap_or(true);
    if !needs_key {
        return Ok(());
    }
    print!("store an API key for `{nick}` now? [Y/n] ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    match line.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => {
            let value = read_secret()?;
            let value = value.trim();
            if value.is_empty() {
                println!("{YELLOW}empty key — skipped{RESET}");
            } else {
                match crate::auth::store(nick, value) {
                    Ok(path) => println!("stored key for {CYAN}{nick}{RESET} -> {}", path.display()),
                    Err(e) => println!("{RED}cannot store key: {e}{RESET}"),
                }
            }
        }
        _ => println!("{DIM}skipped — export the env var before first use{RESET}"),
    }
    Ok(())
}

fn tools_cmd() -> anyhow::Result<u8> {
    println!("{}", ToolRegistry::builtin().catalog());
    Ok(0)
}

fn sessions_cmd() -> anyhow::Result<u8> {
    let summaries = crate::agent::session::list_summaries();
    if summaries.is_empty() {
        println!("no saved sessions yet — chat sessions save automatically after each task");
        return Ok(0);
    }
    for s in summaries {
        let topic = s.topic.unwrap_or_else(|| "-".into());
        println!(
            "{CYAN}{:<24}{RESET} {:>3} msgs  {DIM}{}{RESET}",
            s.name, s.messages, topic
        );
    }
    Ok(0)
}

/// Delete a saved session (`kazi rm <name-prefix>`).
fn rm_cmd(session: &str) -> anyhow::Result<u8> {
    match crate::agent::session::delete(session) {
        Ok(Some(name)) => {
            println!("deleted {name}");
            Ok(0)
        }
        Ok(None) => Ok(0), // unreachable today; keep exhaustive anyway
        Err(e) => {
            println!("{RED}rm failed: {e}{RESET}");
            Ok(1)
        }
    }
}

fn export_cmd(session: &str, out: Option<&Path>) -> anyhow::Result<u8> {
    let name = match crate::agent::session::resolve(session) {
        Some(n) => n,
        None => {
            let have = crate::agent::session::list().join(", ");
            println!(
                "{RED}no saved session matches `{session}`{RESET}{DIM}(stored: {have}){RESET}"
            );
            return Ok(1);
        }
    };
    let f = crate::agent::session::load(&name)?;
    let md = crate::agent::session::export_markdown(&f);
    let target = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{name}.md")));
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, md)?;
    println!("wrote {}", target.display());
    Ok(0)
}
