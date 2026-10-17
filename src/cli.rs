//! CLI surface (clap subcommands) and dispatch into the agent engine.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::agent::{console, context::History, session, tui, Agent, AgentError, AgentEvent};
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
    /// Startup permission mode: read-only | review | edit (default) | yolo
    #[arg(
        long = "permission-mode",
        value_name = "MODE",
        value_parser = parse_permission_mode
    )]
    pub permission_mode: Option<String>,
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
    /// List saved sessions (or search their text with --find)
    #[command(name = "sessions")]
    Sessions {
        /// Search user/assistant text across every saved session
        #[arg(long, value_name = "TEXT")]
        find: Option<String>,
    },
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
        /// Overwrite the target file if it already exists
        #[arg(long)]
        force: bool,
    },
}

/// Manage locally stored API keys (environment variables always win).
#[derive(Subcommand, Debug)]
pub enum AuthCmd {
    /// Store the key for a provider (reads the value from an argument,
    /// stdin, or — interactively — a hidden prompt)
    Set {
        /// Provider nickname from config (see `kazi providers`); omit it on a
        /// terminal to pick one from a list
        provider: Option<String>,
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
        Some(Command::Sessions { find }) => sessions_cmd(find.as_deref()),
        Some(Command::Rm { session }) => rm_cmd(&session),
        Some(Command::Export {
            session,
            out,
            force,
        }) => export_cmd(&session, out.as_deref(), force),
    }
}

async fn chat(
    args: &EngineArgs,
    topic: Option<String>,
    cfg_path: Option<&Path>,
) -> anyhow::Result<u8> {
    // chat is the full-screen TUI and needs a real terminal; pipes and
    // scripts go through `kazi run`, which needs none.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        anyhow::bail!(
            "kazi chat needs a real terminal (stdin and stdout) — for scripts, pipes and CI use `kazi run \"<task>\"`"
        );
    }

    let (cfg, cfg_file) = match config::load(cfg_path) {
        Ok(v) => v,
        Err(config::ConfigError::Missing { path }) => {
            // A truly fresh install: offer to create the file right here
            // instead of bouncing the user to another command first.
            if !interactive {
                return Err(config::ConfigError::Missing { path }.into());
            }
            println!("{YELLOW}no config file yet at {}{RESET}", path.display());
            if !ask_yes("create it from the built-in example now?", true)? {
                return Err(config::ConfigError::Missing { path }.into());
            }
            config::write_example(&path)?;
            println!("{DIM}wrote {}{RESET}", path.display());
            config::load(cfg_path)?
        }
        Err(e) => return Err(e.into()),
    };

    // On a real terminal a missing key should not hard-exit: use a provider
    // that is already ready, or offer to store a key (like claude/codex
    // onboarding). Non-interactive paths keep the fail-fast contract.
    let mut provider = args.provider.clone();
    let mut model = args.model.clone();
    match cfg.resolve(provider.as_deref(), model.as_deref()) {
        Ok(r) => {
            if let Err(e) = cfg.api_key_for(&r.nick) {
                if let Some(alt) = cfg.first_provider_with_key(Some(&r.nick)) {
                    // another provider is ready — use it instead of repeating
                    // the onboarding ceremony on every single launch
                    println!(
                        "{DIM}`{}` has no key — using `{alt}` instead (fix it with: kazi auth set {}){RESET}",
                        r.nick, r.nick
                    );
                    provider = Some(alt);
                    model = None;
                } else if !interactive {
                    return Err(e.into());
                } else if let Some((p, m)) = onboarding_pick(&cfg, &r.nick, Some(&e)).await? {
                    persist_choice(&cfg_file, &p, Some(&m));
                    provider = Some(p);
                    model = Some(m);
                }
            }
        }
        Err(e) => {
            // unknown provider / nothing configured: pick interactively rather
            // than dying, when there is a terminal and something to pick.
            if !interactive || cfg.nicks().is_empty() {
                return Err(e.into());
            }
            match onboarding_pick(&cfg, "", None).await? {
                Some((p, m)) => {
                    persist_choice(&cfg_file, &p, Some(&m));
                    provider = Some(p);
                    model = Some(m);
                }
                None => return Err(e.into()),
            }
        }
    }
    let mode = startup_mode(&cfg, args);
    tui::chat_loop_with_config(
        cfg,
        provider,
        model,
        mode,
        topic,
        args.resume.clone(),
        Some(cfg_file.clone()),
    )
    .await
}

/// Remember a first-run choice in the config file, so the next launch does not
/// ask again. Best-effort: a read-only config must never block the session.
fn persist_choice(cfg_file: &Path, nick: &str, model: Option<&str>) {
    match config::set_default_provider(cfg_file, nick, model) {
        Ok(()) => println!(
            "{DIM}saved `{nick}` as the default in {}{RESET}",
            cfg_file.display()
        ),
        Err(e) => println!(
            "{YELLOW}could not save the choice to {}: {e}{RESET}",
            cfg_file.display()
        ),
    }
}

/// `[Y/n]`-style confirmation for onboarding flows; EOF takes the default.
fn ask_yes(prompt: &str, default_yes: bool) -> anyhow::Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {hint}: ");
    use std::io::Write;
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(default_yes);
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default_yes),
        "y" | "yes" => Ok(true),
        _ => Ok(false),
    }
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

        let cli = Cli::try_parse_from(["kazi", "--model", "x"]).unwrap();
        assert_eq!(cli.engine.model.as_deref(), Some("x"));

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

    #[test]
    fn resolve_pick_handles_numbers_defaults_and_values() {
        let items = vec!["deepseek".to_string(), "ollama".to_string()];
        assert_eq!(resolve_pick("2", &items, 0), Some(1));
        assert_eq!(resolve_pick("", &items, 1), Some(1), "blank = default");
        assert_eq!(resolve_pick("ollama", &items, 0), Some(1), "exact value");
        assert_eq!(resolve_pick("3", &items, 0), None, "out of range");
        assert_eq!(resolve_pick("nope", &items, 0), None);
        assert_eq!(resolve_pick("1", &[], 0), None, "nothing to pick");
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
    // a typo in the config used to silently mean "edit"
    if let Some(raw) = cfg.agent.permission_mode.as_deref() {
        if crate::tools::PermissionMode::parse(raw).is_none() {
            eprintln!(
                "{YELLOW}config agent.permission_mode = \"{raw}\" is not a mode (read-only | review | edit | yolo) — using edit{RESET}"
            );
        }
    }
    Agent::initial_mode(cfg, args.yolo)
}

/// clap validator for `--permission-mode`: a typo fails loudly instead of
/// silently starting in `edit`.
fn parse_permission_mode(s: &str) -> Result<String, String> {
    crate::tools::PermissionMode::parse(s)
        .map(|m| m.as_str().to_string())
        .ok_or_else(|| {
            format!("unknown permission mode `{s}` (allowed: read-only | review | edit | yolo)")
        })
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
    /// USD for this run, when [usage.prices] covers the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
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
    if mode == crate::tools::PermissionMode::Review && fmt == OutputFmt::Text {
        eprintln!(
            "{DIM}note: review mode has no prompt in 'kazi run' — staged changes are accepted automatically (use the TUI for per-file review){RESET}"
        );
    }
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
                    eprintln!("cannot resume `{name}`: {e}");
                    agent
                }
            },
            None => {
                eprintln!("no stored session matches `{raw}`");
                agent
            }
        }
    } else {
        agent
    };

    let mut events: Vec<AgentEvent> = Vec::new();
    let mut on_event: Box<dyn FnMut(AgentEvent)> = match fmt {
        OutputFmt::Text => Box::new(|e| console::render_event(&e)),
        OutputFmt::Json => Box::new(|e| events.push(e)),
    };

    let started = std::time::Instant::now();
    let result = agent.run_task(task, &mut on_event).await;
    let elapsed = started.elapsed();
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
                    cost_usd: agent.cost_usd(),
                    error: None,
                };
                println!("{}", serde_json::to_string(&report).unwrap_or_default());
            } else {
                println!();
                let cost = agent
                    .cost_usd()
                    .map(|c| format!(" · {}", console::money(c)))
                    .unwrap_or_default();
                eprintln!(
                    "{DIM}done in {:.1}s · tokens in {} / out {}{cost}{RESET}",
                    elapsed.as_secs_f64(),
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0)
                );
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
                    cost_usd: agent.cost_usd(),
                    error: Some(e.to_string()),
                };
                println!("{}", serde_json::to_string(&report).unwrap_or_default());
            } else {
                eprintln!("\n{YELLOW}{held}{RESET}\n{RED}{e}{RESET}");
                eprintln!(
                    "{DIM}failed after {:.1}s · tokens in {} / out {}{RESET}",
                    elapsed.as_secs_f64(),
                    usage.input_tokens.unwrap_or(0),
                    usage.output_tokens.unwrap_or(0)
                );
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
        println!("next steps:");
        println!("  1. just run `kazi` — it walks you through provider/model and stores a key");
        println!("  2. or edit the file by hand, then make a key available:");
        println!("       export <PROVIDER>_API_KEY=...   (env vars always win)");
        println!("       kazi auth set <provider>        (stored locally, see `kazi providers`)");
        return Ok(0);
    }
    println!("config path: {}", path.display());
    if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(t) => println!("\n{DIM}--- contents ---{RESET}\n{t}"),
            Err(e) => println!("{RED}cannot read {path:?}: {e}{RESET}"),
        }
    } else {
        println!("{YELLOW}no file there yet — create it with: kazi config --init{RESET}");
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
        let key = if !cfg.needs_auth(nick) {
            "no key needed".to_string()
        } else {
            match cfg.kind_of_nick(nick) {
                Ok(kind) => {
                    let env_name = entry
                        .api_key_env
                        .clone()
                        .or_else(|| config::Config::default_key_env(kind).map(str::to_string))
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
                        format!(
                            "{YELLOW}key: missing — export {env_name} or `kazi auth set {nick}`{RESET}"
                        )
                    }
                }
                Err(_) => String::new(),
            }
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
            let path = crate::auth::auth_path()?;
            if store.keys.is_empty() {
                println!(
                    "no stored keys yet — the store would be {}{DIM}(env vars always win over it){RESET}",
                    path.display()
                );
                // name the providers: "<provider>" is not a usable instruction
                if let Ok((cfg, _)) = config::load(cfg_path) {
                    let need: Vec<String> = cfg
                        .nicks()
                        .into_iter()
                        .filter(|n| cfg.needs_auth(n))
                        .collect();
                    if !need.is_empty() {
                        println!("providers that need one: {}", need.join(", "));
                        println!("store one with: {CYAN}kazi auth set <provider>{RESET}");
                    }
                }
                return Ok(0);
            }
            println!(
                "{DIM}stored keys (env vars still win): {}{RESET}",
                path.display()
            );
            for nick in store.keys.keys() {
                println!("  {CYAN}{nick}{RESET}");
            }
            Ok(0)
        }
        AuthCmd::Set { provider, key } => {
            let (cfg, _) = config::load(cfg_path)?;
            let nick = match provider {
                Some(p) => resolve_provider_nick(&cfg, &p)?,
                None => {
                    // pick interactively from the providers that need a key
                    let candidates: Vec<String> = cfg
                        .nicks()
                        .into_iter()
                        .filter(|n| {
                            cfg.kind_of_nick(n)
                                .map(|k| k != crate::llm::ProviderKind::Ollama)
                                .unwrap_or(true)
                        })
                        .collect();
                    if !std::io::stdin().is_terminal() || candidates.is_empty() {
                        anyhow::bail!(
                            "usage: kazi auth set <provider> [KEY] — run `kazi providers` to list them"
                        );
                    }
                    // default to the provider the config actually selects, not
                    // whatever sorts first alphabetically
                    let default = cfg
                        .resolve(None, None)
                        .ok()
                        .and_then(|r| candidates.iter().position(|c| c == &r.nick))
                        .unwrap_or(0);
                    match pick_one("pick a provider to store a key for", &candidates, default) {
                        Some(i) => candidates[i].clone(),
                        None => return Ok(1),
                    }
                }
            };
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

/// Map a raw picker line to an item index: blank → `default`, a 1-based number
/// or an exact value also work. `None` when nothing matches.
fn resolve_pick(input: &str, items: &[String], default: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let default = default.min(items.len() - 1);
    let t = input.trim();
    if t.is_empty() {
        return Some(default);
    }
    match t.parse::<usize>() {
        Ok(n) if n >= 1 && n <= items.len() => Some(n - 1),
        _ => items.iter().position(|it| it == t), // exact value
    }
}

/// Numbered picker for the onboarding flows (first run, `kazi auth set`):
/// prints the items and reads one line from stdin. Blank takes the default,
/// `q` cancels; `None` on EOF or nothing to choose.
fn pick_one(prompt: &str, items: &[String], default: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let default = default.min(items.len() - 1);
    println!("{CYAN}{prompt}{RESET}");
    for (i, it) in items.iter().enumerate() {
        let mark = if i == default { "  (default)" } else { "" };
        println!("  {}) {it}{mark}", i + 1);
    }
    print!(
        "choose [1-{}, Enter = {}, q = cancel]: ",
        items.len(),
        default + 1
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    if matches!(line.trim().to_ascii_lowercase().as_str(), "q" | "quit") {
        println!("{DIM}cancelled{RESET}");
        return None;
    }
    resolve_pick(&line, items, default)
}

/// Interactive provider + model chooser for first-run onboarding: picks a
/// provider, a model for it (config + live + defaults), then stores the API
/// key when the provider needs one. Returns the `(provider, model)` to use for
/// this session, or `None` when there is nothing to pick.
async fn onboarding_pick(
    cfg: &config::Config,
    default_nick: &str,
    warn: Option<&config::ConfigError>,
) -> anyhow::Result<Option<(String, String)>> {
    if let Some(e) = warn {
        println!("{YELLOW}warning: {e}{RESET}");
    }
    let nicks = cfg.nicks();
    if nicks.is_empty() {
        println!(
            "{YELLOW}no providers configured — run `kazi config --init`, then edit the file{RESET}"
        );
        return Ok(None);
    }
    let def = nicks.iter().position(|n| n == default_nick).unwrap_or(0);
    let Some(pi) = pick_one("pick a provider", &nicks, def) else {
        return Ok(None);
    };
    let nick = nicks[pi].clone();
    let kind = cfg
        .kind_of_nick(&nick)
        .unwrap_or(crate::llm::ProviderKind::OpenAiCompat);

    let configured = cfg.providers.get(&nick).and_then(|e| e.model.clone());
    let models = crate::agent::console::model_catalog(cfg, &nick, configured.as_deref()).await;
    let model = if models.is_empty() {
        configured.clone().unwrap_or_default()
    } else {
        let mdef = configured
            .as_ref()
            .and_then(|c| models.iter().position(|m| m == c))
            .unwrap_or(0);
        match pick_one("pick a model", &models, mdef) {
            Some(mi) => models[mi].clone(),
            None => configured.clone().unwrap_or_default(),
        }
    };
    if model.is_empty() {
        println!("{YELLOW}no model chosen — set `model` in config or pass --model{RESET}");
        return Ok(None);
    }

    // store a key unless the provider is keyless (ollama) or already has one
    if kind != crate::llm::ProviderKind::Ollama && cfg.api_key_for(&nick).is_err() {
        let value = read_secret()?;
        let value = value.trim();
        if value.is_empty() {
            println!("{DIM}skipped — export the env var before first use{RESET}");
        } else {
            match crate::auth::store(&nick, value) {
                Ok(path) => println!("stored key for {CYAN}{nick}{RESET} -> {}", path.display()),
                Err(e) => println!("{RED}cannot store key: {e}{RESET}"),
            }
        }
    }
    Ok(Some((nick, model)))
}

fn tools_cmd() -> anyhow::Result<u8> {
    println!("{}", ToolRegistry::builtin().catalog());
    Ok(0)
}

fn sessions_cmd(find: Option<&str>) -> anyhow::Result<u8> {
    if let Some(text) = find {
        let hits = crate::agent::session::search(text, 3, 40);
        if hits.is_empty() {
            println!("no saved session mentions {text:?} (user/assistant text only)");
            return Ok(0);
        }
        println!("{} hit(s) for {text:?}:", hits.len());
        for h in hits {
            println!(
                "{CYAN}{}{RESET}  {DIM}{} line {}{RESET}  {}",
                h.session, h.role, h.line, h.snippet
            );
        }
        println!("{DIM}resume one with: kazi chat --resume <session>{RESET}");
        return Ok(0);
    }
    let summaries = crate::agent::session::list_summaries();
    if summaries.is_empty() {
        println!("no saved sessions yet — chat sessions save automatically after each task");
        return Ok(0);
    }
    for s in summaries {
        let topic: String = s
            .topic
            .unwrap_or_else(|| "-".into())
            .replace(['\n', '\r'], " ");
        let age = age_label(s.saved_at);
        let cwd = s
            .cwd
            .as_deref()
            .map(|c| format!("  {DIM}in {c}{RESET}"))
            .unwrap_or_default();
        println!(
            "{CYAN}{:<24}{RESET} {:>3} msgs  {DIM}{age:>4}{RESET}  {topic}{cwd}",
            s.name, s.messages
        );
    }
    Ok(0)
}

/// Human age label for a save timestamp (`3m`, `4h`, `2d`).
fn age_label(saved_at: u64) -> String {
    let secs = crate::agent::session::now_secs().saturating_sub(saved_at);
    match secs {
        0..=59 => "now".to_string(),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
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

fn export_cmd(session: &str, out: Option<&Path>, force: bool) -> anyhow::Result<u8> {
    let name = match crate::agent::session::resolve(session) {
        Some(n) => n,
        None => {
            let matches: Vec<String> = crate::agent::session::list()
                .into_iter()
                .filter(|n| n.starts_with(session))
                .collect();
            let why = if matches.len() > 1 {
                format!("`{session}` is ambiguous — matches: {}", matches.join(", "))
            } else {
                format!("no saved session matches `{session}`")
            };
            println!("{RED}{why}{RESET}");
            return Ok(1);
        }
    };
    let f = crate::agent::session::load(&name)?;
    let md = crate::agent::session::export_markdown(&f);
    let target = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{name}.md")));
    if target.exists() && !force {
        println!(
            "{RED}{} already exists — pass --force to overwrite{RESET}",
            target.display()
        );
        return Ok(1);
    }
    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, md)?;
    println!("wrote {}", target.display());
    Ok(0)
}
