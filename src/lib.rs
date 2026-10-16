//! # kazi — a tiny, from-scratch AI agent
//!
//! `kazi` gives terminal-driving AI agents to any OpenAI-compatible API,
//! Anthropic Claude, or local Ollama — implemented from raw HTTP + JSON, with
//! no agent framework underneath. The crate ships two halves:
//!
//! - a **library** (this crate): the engine ([`agent::Agent`]), provider
//!   abstraction ([`llm::Provider`]), and tool system ([`tools`]) — embed
//!   them into your own CLI or service;
//! - a **binary** `kazi`: `kazi chat` REPL and `kazi run "task"` headless mode.
//!
//! ## Library quick look
//!
//! ```no_run
//! use kazi::agent::{Agent, repl};
//!
//! # async fn demo() -> anyhow::Result<()> {
//! // config.toml: [providers.deepseek] type = "openai" ...
//! let cfg = kazi::config::load(None).expect("config").0;
//! let mut agent = Agent::default_cli(&cfg, Some("deepseek"), None, false)
//!     .await
//!     .expect("provider wired");
//!
//! let answer = agent
//!     .run_task("How many .rs files are in src?", |event| repl::render_event(&event))
//!     .await
//!     .expect("task");
//! println!("{answer}");
//! # Ok(())
//! # }
//! #
//! # fn main() -> anyhow::Result<()> {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(demo())
//! # }
//! ```
//!
//! ## Design map
//!
//! | module    | role                                                        |
//! |-----------|--------------------------------------------------------------|
//! | [`llm`]   | one [`llm::Provider`] trait; wire clients for 3 protocols     |
//! | [`tools`] | [`tools::Tool`] trait + built-ins; approvals + sandboxing     |
//! | [`agent`] | the loop ([`agent::Agent`]), history trimming, sessions, REPL |
//! | [`config`]| TOML config with provider precedence + env-key resolution     |

pub mod agent;
pub mod cli;
pub mod config;
pub mod llm;
pub mod mcp;
pub mod tools;

/// Crate version (matches `CARGO_PKG_VERSION`), handy for embedders.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
