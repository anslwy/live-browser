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
                // NOTE: the binary crate is named `lb` ([[bin]] in Cargo.toml),
                // so `live_browser=…` alone would silence every tracing event.
                .unwrap_or_else(|_| "lb=info,live_browser=info".into()),
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
    let tab = cli.tab;

    match cli.command {
        Commands::Daemon(args) => daemon::run_daemon(args).await,
        Commands::Open { url, json } => client::open(&server, &url, tab, json).await,
        Commands::Status { json } => client::status(&server, tab, json).await,
        Commands::List { json } => client::list(&server, json).await,
        Commands::Close { tab: t, json } => client::close(&server, t.or(tab), json).await,
        Commands::Use { tab: t, json } => client::use_tab(&server, t, json).await,
        Commands::Snapshot { json, full } => client::snapshot(&server, tab, json, full).await,
        Commands::Shot { out } => client::shot(&server, tab, Some(out.as_str())).await,
        Commands::Click { target, json } => client::click(&server, &target, tab, json).await,
        Commands::ClickAt { x, y, json } => client::click_at(&server, x, y, tab, json).await,
        Commands::Hover { x, y, json } => client::hover(&server, x, y, tab, json).await,
        Commands::Scroll { dx, dy, json } => client::scroll(&server, dx, dy, tab, json).await,
        Commands::Wheel { x, y, dy, dx, json } => {
            client::wheel(&server, x, y, dx, dy, tab, json).await
        }
        Commands::Drag { x1, y1, x2, y2, steps, json } => {
            client::drag(&server, x1, y1, x2, y2, steps, tab, json).await
        }
        Commands::Zoom { action, json } => client::zoom(&server, &action, tab, json).await,
        Commands::Type {
            target,
            text,
            submit,
            json,
        } => client::type_text(&server, &target, &text, submit, tab, json).await,
        Commands::Press { key, json } => client::press(&server, &key, tab, json).await,
        Commands::Select { target, value, json } => {
            client::select(&server, &target, &value, tab, json).await
        }
        Commands::Insert { text, json } => client::insert(&server, &text, tab, json).await,
        Commands::Eval { js, json } => client::eval(&server, &js, tab, json).await,
        Commands::Back { json } => client::nav(&server, "back", tab, json).await,
        Commands::Forward { json } => client::nav(&server, "forward", tab, json).await,
        Commands::Reload { json } => client::nav(&server, "reload", tab, json).await,
        Commands::Handover { message, timeout } => {
            client::handover(&server, &message, tab, timeout).await
        }
        Commands::HandoverDone { json } => client::handover_done(&server, tab, json).await,
        Commands::Claim { message, json } => client::claim(&server, &message, tab, json).await,
        Commands::Log { json } => client::log(&server, json).await,
    }
}
