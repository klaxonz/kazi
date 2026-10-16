use std::process::ExitCode;

use clap::Parser as _;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = kazi::cli::Cli::parse();
    match kazi::cli::execute(cli).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{e:#}");
            for cause in e.chain().skip(1) {
                eprintln!("  caused by: {cause}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Structured logging is opt-in via `KAZI_LOG` (e.g. `KAZI_LOG=debug cargo run`).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
