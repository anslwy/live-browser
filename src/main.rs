mod cli;
mod client;
mod daemon;

use anyhow::Result;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "live_browser=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = <Cli as clap::Parser>::parse();
    // Server precedence: --server flag > LB_SERVER > SB_SERVER (legacy) > default.
    let server = cli
        .server
        .clone()
        .or_else(|| std::env::var("SB_SERVER").ok())
        .unwrap_or_else(|| "http://127.0.0.1:8765".to_string());

    match cli.command {
        Commands::Daemon(args) => daemon::run_daemon(args).await,
        Commands::Open { url, json } => client::open(&server, &url, json).await,
        Commands::Status { json } => client::status(&server, json).await,
        Commands::Snapshot { json, full } => client::snapshot(&server, json, full).await,
        Commands::Shot { out } => client::shot(&server, Some(out.as_str())).await,
        Commands::Click { target, json } => client::click(&server, &target, json).await,
        Commands::ClickAt { x, y, json } => client::click_at(&server, x, y, json).await,
        Commands::Hover { x, y, json } => client::hover(&server, x, y, json).await,
        Commands::Scroll { dx, dy, json } => client::scroll(&server, dx, dy, json).await,
        Commands::Wheel { x, y, dy, dx, json } => client::wheel(&server, x, y, dx, dy, json).await,
        Commands::Drag { x1, y1, x2, y2, steps, json } => {
            client::drag(&server, x1, y1, x2, y2, steps, json).await
        }
        Commands::Zoom { action, json } => client::zoom(&server, &action, json).await,
        Commands::Type {
            target,
            text,
            submit,
            json,
        } => client::type_text(&server, &target, &text, submit, json).await,
        Commands::Press { key, json } => client::press(&server, &key, json).await,
        Commands::Insert { text, json } => client::insert(&server, &text, json).await,
        Commands::Select { target, value, json } => client::select(&server, &target, &value, json).await,
        Commands::Eval { js, json } => client::eval(&server, &js, json).await,
        Commands::Back { json } => client::nav(&server, "back", json).await,
        Commands::Forward { json } => client::nav(&server, "forward", json).await,
        Commands::Reload { json } => client::nav(&server, "reload", json).await,
        Commands::Handover { message, timeout } => {
            client::handover(&server, &message, timeout).await
        }
        Commands::HandoverDone { json } => client::handover_done(&server, json).await,
        Commands::Claim { message, json } => client::claim(&server, &message, json).await,
        Commands::Log { json } => client::log(&server, json).await,
    }
}
