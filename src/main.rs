mod cli;
mod client;
mod daemon;
mod webctl;

use anyhow::Result;
use cli::{Cli, Commands, WebAction};

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
    // Passcode precedence: --passcode flag > LB_PASSCODE > LB_PASSWORD (compat) > none.
    let passcode = cli
        .passcode
        .clone()
        .or_else(|| std::env::var("LB_PASSWORD").ok())
        .filter(|s| !s.is_empty());
    let pass = passcode.as_deref();

    match cli.command {
        Commands::Web(w) => match w.action {
            None => {
                webctl::ensure_not_running(w.daemon.port)?;
                daemon::run_daemon(w.daemon, passcode).await
            }
            Some(WebAction::Stop) => {
                println!("{}", webctl::stop(w.daemon.port, w.daemon.profile.as_deref())?);
                Ok(())
            }
            Some(WebAction::Restart) => {
                match webctl::stop(w.daemon.port, w.daemon.profile.as_deref()) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => println!("stop skipped: {e:#}"),
                }
                webctl::ensure_not_running(w.daemon.port)?;
                daemon::run_daemon(w.daemon, passcode).await
            }
        },
        Commands::Open { url, json } => client::open(&server, pass, &url, tab, json).await,
        Commands::Status { json } => client::status(&server, pass, tab, json).await,
        Commands::List { json } => client::list(&server, pass, json).await,
        Commands::Close { tab: t, json } => client::close(&server, pass, t.or(tab), json).await,
        Commands::Use { tab: t, json } => client::use_tab(&server, pass, t, json).await,
        Commands::Snapshot { json, full } => client::snapshot(&server, pass, tab, json, full).await,
        Commands::Shot { out } => client::shot(&server, pass, tab, Some(out.as_str())).await,
        Commands::Click { target, json } => client::click(&server, pass, &target, tab, json).await,
        Commands::ClickAt { x, y, json } => client::click_at(&server, pass, x, y, tab, json).await,
        Commands::Hover { x, y, json } => client::hover(&server, pass, x, y, tab, json).await,
        Commands::Scroll { dx, dy, json } => client::scroll(&server, pass, dx, dy, tab, json).await,
        Commands::Wheel { x, y, dy, dx, json } => {
            client::wheel(&server, pass, x, y, dx, dy, tab, json).await
        }
        Commands::Drag { x1, y1, x2, y2, steps, json } => {
            client::drag(&server, pass, x1, y1, x2, y2, steps, tab, json).await
        }
        Commands::Zoom { action, json } => client::zoom(&server, pass, &action, tab, json).await,
        Commands::Type {
            target,
            text,
            submit,
            json,
        } => client::type_text(&server, pass, &target, &text, submit, tab, json).await,
        Commands::Press { key, json } => client::press(&server, pass, &key, tab, json).await,
        Commands::Select { target, value, json } => {
            client::select(&server, pass, &target, &value, tab, json).await
        }
        Commands::Insert { text, json } => client::insert(&server, pass, &text, tab, json).await,
        Commands::Eval { js, json } => client::eval(&server, pass, &js, tab, json).await,
        Commands::Back { json } => client::nav(&server, pass, "back", tab, json).await,
        Commands::Forward { json } => client::nav(&server, pass, "forward", tab, json).await,
        Commands::Reload { json } => client::nav(&server, pass, "reload", tab, json).await,
        Commands::Handover { message, timeout } => {
            client::handover(&server, pass, &message, tab, timeout).await
        }
        Commands::HandoverDone { json } => client::handover_done(&server, pass, tab, json).await,
        Commands::Claim { message, json } => client::claim(&server, pass, &message, tab, json).await,
        Commands::Log { json } => client::log(&server, pass, json).await,
    }
}
